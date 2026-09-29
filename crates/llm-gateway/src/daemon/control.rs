//! 走っている台の制御口 (DR-0032 決定 2・3)。
//!
//! 台は `daemon/control/<unit>.sock` で待ち、監督者だけがそこへ繋いで
//! 設定の読み直しを命じる。子の HTTP (Caddy 越しに外から届く面) には
//! 台の挙動を変える操作を置かない。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::protocol::{self, ControlRequest, ReloadFailure, Reloaded};
use crate::credential::CredentialPersistence;
use crate::gateway::ReloadError;
use crate::{Config, Gateway};

/// 開いている制御口。落とすと socket も消す。
pub struct ControlSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl ControlSocket {
    /// 既定の置き場に、この台の制御口を開く。
    pub fn open(unit: &str) -> std::io::Result<Self> {
        Self::open_at(&protocol::control_path(&protocol::control_dir(), unit))
    }

    /// `path` に開く。前回の残骸があれば消してから開く。
    pub fn open_at(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            // 既にあったディレクトリも閉じる。頼めば設定が差し替わる口の置き場。
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 頼みに答え続ける。1 本の繋がりに 1 つの頼み。
    ///
    /// 読み直しは 1 つずつ通す。同時に 2 つ来たとき、後から読んだ設定が
    /// 先に差し替わって古い方で上書きされる、を起こさない。
    pub async fn serve<P: CredentialPersistence + 'static>(
        &self,
        unit: &str,
        config_path: &Path,
        gateway: Arc<Gateway<P>>,
    ) {
        let one_at_a_time = tokio::sync::Mutex::new(());
        loop {
            let stream = match self.listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::error!(%e, socket = %self.path.display(), "stopped accepting control requests");
                    return;
                }
            };
            let _held = one_at_a_time.lock().await;
            answer(stream, unit, config_path, &gateway).await;
        }
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn answer<P: CredentialPersistence>(
    stream: UnixStream,
    unit: &str,
    config_path: &Path,
    gateway: &Gateway<P>,
) {
    let (reading, mut writing) = stream.into_split();
    let Ok(Some(line)) = BufReader::new(reading).lines().next_line().await else {
        return;
    };
    let result = match serde_json::from_str::<ControlRequest>(&line) {
        Ok(ControlRequest::Reload) => reload(unit, config_path, gateway).await,
        Err(e) => Reloaded::failed(
            unit,
            "bad_request",
            format!("could not read the request: {e}"),
        ),
    };
    let mut text = serde_json::to_string(&result).unwrap_or_default();
    text.push('\n');
    let _ = writing.write_all(text.as_bytes()).await;
}

/// 設定を読み直す。`check` と同じ経路で全体を読み、通った時だけ差し替える
/// (DR-0032 決定 3)。
pub async fn reload<P: CredentialPersistence>(
    unit: &str,
    config_path: &Path,
    gateway: &Gateway<P>,
) -> Reloaded {
    let config = match Config::load(config_path) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(%e, "the configuration was not reloaded");
            return Reloaded::failed(
                unit,
                "invalid_config",
                format!(
                    "{e}; still running the previous configuration. fix {} and reload again",
                    config_path.display()
                ),
            );
        }
    };
    match gateway.reload(config).await {
        Ok(outcome) => Reloaded::done(unit, outcome.warnings),
        Err(e @ ReloadError::RestartRequired { .. }) => {
            tracing::warn!(%e, "the configuration was not reloaded");
            Reloaded {
                unit: unit.to_owned(),
                ok: false,
                error: Some(ReloadFailure {
                    kind: "restart_required".to_owned(),
                    message: e.to_string(),
                    fields: e.fields().into_iter().map(str::to_owned).collect(),
                }),
                warnings: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::file::FileStore;

    struct Unit {
        dir: tempfile::TempDir,
        config: PathBuf,
        gateway: Arc<Gateway<FileStore>>,
    }

    fn body(dir: &Path, listen: &str, extra: &str) -> String {
        format!(
            r#"
[server]
listen = "{listen}"

[store]
type = "file"
dir = {store:?}

[stats]
dir = {stats:?}

[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:1"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
{extra}"#,
            store = dir.join("credentials"),
            stats = dir.join("stats"),
        )
    }

    fn unit() -> Unit {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("u.toml");
        std::fs::write(&config, body(dir.path(), "127.0.0.1:11301", "")).unwrap();
        let loaded = Config::load(&config).unwrap();
        let store = FileStore::open(dir.path().join("credentials")).unwrap();
        let gateway = Arc::new(Gateway::new(&loaded, store).unwrap());
        Unit {
            dir,
            config,
            gateway,
        }
    }

    const OTHER_NS: &str = "\n[[ns.other.routing]]\nmodels = [\"m\"]\nroutes = [\"a\"]\n";

    /// 通る設定なら差し替わり、次から新しい設定が効く。
    #[tokio::test]
    async fn a_valid_configuration_is_swapped_in() {
        let u = unit();
        std::fs::write(&u.config, body(u.dir.path(), "127.0.0.1:11301", OTHER_NS)).unwrap();

        let result = reload("u", &u.config, &u.gateway).await;

        assert!(result.ok, "{result:?}");
        assert_eq!(result.error, None);
        assert!(u.gateway.namespace("other").is_some());
    }

    /// 検証に落ちる設定は何も差し替えず、理由を返す。
    #[tokio::test]
    async fn an_invalid_configuration_keeps_the_running_one() {
        let u = unit();
        std::fs::write(
            &u.config,
            body(
                u.dir.path(),
                "127.0.0.1:11301",
                "\n[[ns.other.routing]]\nmodels = [\"m\"]\nroutes = [\"nowhere\"]\n",
            ),
        )
        .unwrap();

        let result = reload("u", &u.config, &u.gateway).await;

        assert!(!result.ok);
        let error = result.error.unwrap();
        assert_eq!(error.kind, "invalid_config");
        assert!(error.message.contains("nowhere"), "{}", error.message);
        assert!(
            u.gateway.namespace("other").is_none(),
            "nothing was swapped"
        );
    }

    /// 変えられない欄が変わっていれば、他の欄も含めて差し替えない。
    #[tokio::test]
    async fn a_changed_fixed_field_keeps_the_running_configuration() {
        let u = unit();
        std::fs::write(&u.config, body(u.dir.path(), "127.0.0.1:11303", OTHER_NS)).unwrap();

        let result = reload("u", &u.config, &u.gateway).await;

        assert!(!result.ok);
        let error = result.error.unwrap();
        assert_eq!(error.kind, "restart_required");
        assert_eq!(error.fields, ["[server] listen"]);
        assert!(
            error
                .message
                .contains("[server] listen changed (127.0.0.1:11301 -> 127.0.0.1:11303)"),
            "{}",
            error.message
        );
        assert!(
            u.gateway.namespace("other").is_none(),
            "nothing was swapped"
        );
    }

    /// 監督者と同じ言葉 (JSON 1 行) で socket 越しに答え、閉じたら socket も消す。
    #[tokio::test]
    async fn the_control_socket_answers_in_one_line_and_is_removed_when_closed() {
        let u = unit();
        let path = u.dir.path().join("control").join("u.sock");
        let socket = Arc::new(ControlSocket::open_at(&path).unwrap());
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(&path), 0o600);
        }

        let serving = Arc::clone(&socket);
        let (config, gateway) = (u.config.clone(), Arc::clone(&u.gateway));
        let task = tokio::spawn(async move { serving.serve("u", &config, gateway).await });

        let answer = protocol::ask(&path, &ControlRequest::Reload).await.unwrap();
        assert_eq!(answer, r#"{"unit":"u","ok":true}"#);

        task.abort();
        let _ = task.await;
        drop(Arc::try_unwrap(socket).ok().unwrap());
        assert!(!path.exists());
    }
}
