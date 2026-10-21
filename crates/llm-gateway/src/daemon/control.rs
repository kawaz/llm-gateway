//! 走っている台の制御口 (DR-0032 決定 2・3)。
//!
//! 台は `daemon/control/<unit>.sock` で待ち、監督者だけがそこへ繋いで
//! 設定の読み直しと一時ダンプ (DR-0038 決定 2) を頼む。子の HTTP (Caddy 越しに
//! 外から届く面) には台の挙動を変える操作を置かない。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::protocol::{self, ControlRequest, DumpRequest, ReloadFailure, Reloaded};
use crate::credential::CredentialPersistence;
use crate::credential::time::now_unix_ms;
use crate::gateway::ReloadError;
use crate::{Config, Gateway};

/// 頼みの 1 行を待つ上限。
const REQUEST_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// 頼みの 1 行の長さの上限 (バイト)。頼みは `{"op":"reload"}` 程度。
const REQUEST_LIMIT: u64 = 64 * 1024;

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
    /// 頼みは 1 つずつ通す。読み直しが同時に 2 つ来たとき、後から読んだ設定が
    /// 先に差し替わって古い方で上書きされる、を起こさない。
    pub async fn serve<P: CredentialPersistence + 'static>(
        &self,
        unit: &str,
        config_path: &Path,
        gateway: Arc<Gateway<P>>,
    ) {
        loop {
            let stream = match self.listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::error!(%e, socket = %self.path.display(), "stopped accepting control requests");
                    return;
                }
            };
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
    // 読み直しは 1 つずつ通すので、改行を寄越さない繋がりが 1 本あると
    // 監督者の頼みが後ろで待たされる。1 行を読む時間と長さに上限を置く。
    let mut line = Vec::new();
    let mut limited = BufReader::new(reading).take(REQUEST_LIMIT + 1);
    match tokio::time::timeout(REQUEST_WAIT, limited.read_until(b'\n', &mut line)).await {
        Ok(Ok(_)) if line.ends_with(b"\n") => {}
        _ => return,
    }
    let line = String::from_utf8_lossy(&line);
    let result = match serde_json::from_str::<ControlRequest>(&line) {
        Ok(ControlRequest::Reload) => {
            serde_json::to_value(reload(unit, config_path, gateway).await).unwrap_or_default()
        }
        Ok(ControlRequest::Dump(request)) => dump(unit, request, gateway, now_unix_ms()),
        Err(e) => serde_json::to_value(Reloaded::failed(
            unit,
            "bad_request",
            format!("could not read the request: {e}"),
        ))
        .unwrap_or_default(),
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

/// 一時ダンプの頼みに答える (DR-0038 決定 2・11)。
///
/// 答えはどれも `unit` と `ok` で始まり、`ok: true` の答えには系列の欄を同じ
/// 階層に添える。断る時は `ok: false` と `error: {kind, message}`。
pub fn dump<P: CredentialPersistence>(
    unit: &str,
    request: DumpRequest,
    gateway: &Gateway<P>,
    now: i64,
) -> serde_json::Value {
    let dumps = gateway.dumps();
    let answered = match request {
        DumpRequest::Start {
            ns,
            for_secs,
            max_bytes,
            request_only,
        } => {
            // 走っている設定に無い ns は書く相手が無い (決定 4)。
            if gateway.namespace(&ns).is_none() {
                let known = gateway.namespace_names().join(", ");
                return dump_failed(
                    unit,
                    "unknown_namespace",
                    format!("this unit has no namespace `{ns}` (it has: {known})"),
                );
            }
            let start = crate::dump::Start {
                unit,
                ns: &ns,
                for_ms: i64::try_from(for_secs)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1000),
                max_bytes,
                request_only,
            };
            dumps.start(&start, now).map(|view| fields(&view))
        }
        DumpRequest::Stop {
            dump_id: Some(id),
            all: false,
        } => dumps.stop(&id).map(|view| fields(&view)),
        DumpRequest::Stop {
            dump_id: None,
            all: true,
        } => Ok(listed(&dumps.stop_all(now))),
        DumpRequest::Stop { .. } => {
            return dump_failed(unit, "bad_request", "say one dump_id, or --all");
        }
        DumpRequest::Status => Ok(listed(&dumps.status(now))),
    };
    match answered {
        Ok(fields) => {
            let mut answer = serde_json::Map::new();
            answer.insert("unit".to_owned(), serde_json::json!(unit));
            answer.insert("ok".to_owned(), serde_json::json!(true));
            answer.extend(fields);
            serde_json::Value::Object(answer)
        }
        Err(refusal) => dump_failed(unit, refusal.kind, refusal.message),
    }
}

/// 系列 1 つの欄。
fn fields(view: &crate::dump::View) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::to_value(view) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

/// 系列の一覧を `dumps` に並べる。
fn listed(views: &[crate::dump::View]) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("dumps".to_owned(), serde_json::json!(views));
    map
}

fn dump_failed(unit: &str, kind: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "unit": unit,
        "ok": false,
        "error": { "kind": kind, "message": message.into() },
    })
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

        // 改行を寄越さない繋がりがあっても、上限で閉じて次の頼みに答える。
        let mut silent = UnixStream::connect(&path).await.unwrap();
        silent.write_all(b"{\"op\":").await.unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(
            REQUEST_WAIT * 2,
            tokio::io::AsyncReadExt::read_to_end(&mut silent, &mut rest),
        )
        .await
        .expect("closed at the limit")
        .unwrap();
        assert!(rest.is_empty());

        let answer = protocol::ask(&path, &ControlRequest::Reload).await.unwrap();
        assert_eq!(answer, r#"{"unit":"u","ok":true}"#);

        task.abort();
        let _ = task.await;
        drop(Arc::try_unwrap(socket).ok().unwrap());
        assert!(!path.exists());
    }

    fn dump_start(ns: &str) -> DumpRequest {
        DumpRequest::Start {
            ns: ns.to_owned(),
            for_secs: 600,
            max_bytes: None,
            request_only: false,
        }
    }

    /// 答えは `unit` と `ok` で始まり、系列の欄を同じ階層に添える (DR-0038 決定 11)。
    #[tokio::test]
    async fn a_dump_is_started_listed_and_stopped_through_the_control_socket() {
        let u = unit();
        let path = u.dir.path().join("control").join("u.sock");
        let socket = Arc::new(ControlSocket::open_at(&path).unwrap());
        let serving = Arc::clone(&socket);
        let (config, gateway) = (u.config.clone(), Arc::clone(&u.gateway));
        let task = tokio::spawn(async move { serving.serve("u", &config, gateway).await });

        let ask = |request: DumpRequest| {
            let path = path.clone();
            async move {
                let line = protocol::ask(&path, &ControlRequest::Dump(request))
                    .await
                    .unwrap();
                serde_json::from_str::<serde_json::Value>(&line).unwrap()
            }
        };

        let started = ask(dump_start("default")).await;
        assert_eq!(started["unit"], "u");
        assert_eq!(started["ok"], true);
        assert_eq!(started["ns"], "default");
        assert_eq!(started["max_bytes"], crate::dump::DEFAULT_MAX_BYTES);
        assert_eq!(started["request_only"], false);
        assert!(started.get("bytes_written").is_none(), "{started}");
        let id = started["dump_id"].as_str().unwrap().to_owned();
        let dir = started["dump_dir"].as_str().unwrap();
        assert!(
            dir.ends_with(&format!("stats/dumps/u/default/{id}")),
            "{dir}"
        );

        let listed = ask(DumpRequest::Status).await;
        assert_eq!(listed["ok"], true);
        assert_eq!(listed["dumps"][0]["dump_id"], id.as_str());
        assert_eq!(listed["dumps"][0]["requests"], 0);

        let stopped = ask(DumpRequest::Stop {
            dump_id: Some(id.clone()),
            all: false,
        })
        .await;
        assert_eq!(stopped["ok"], true);
        assert_eq!(stopped["active"], false);
        assert_eq!(
            ask(DumpRequest::Status).await,
            serde_json::json!({"unit": "u", "ok": true, "dumps": []})
        );
        assert_eq!(
            ask(DumpRequest::Stop {
                dump_id: None,
                all: true
            })
            .await,
            serde_json::json!({"unit": "u", "ok": true, "dumps": []})
        );

        task.abort();
        let _ = task.await;
    }

    /// 走っている設定に無い ns、知らない id、どちらも指さない stop は断る。
    #[tokio::test]
    async fn a_dump_request_that_points_at_nothing_is_refused() {
        let u = unit();
        let now = now_unix_ms();
        let kind = |answer: serde_json::Value| {
            assert_eq!(answer["ok"], false, "{answer}");
            assert_eq!(answer["unit"], "u", "{answer}");
            answer["error"]["kind"].as_str().unwrap().to_owned()
        };

        let unknown = dump("u", dump_start("nope"), &u.gateway, now);
        assert!(
            unknown["error"]["message"]
                .as_str()
                .unwrap()
                .contains("default"),
            "{unknown}"
        );
        assert_eq!(kind(unknown), "unknown_namespace");
        assert_eq!(
            kind(dump(
                "u",
                DumpRequest::Stop {
                    dump_id: Some("019a".to_owned()),
                    all: false
                },
                &u.gateway,
                now
            )),
            "unknown_dump"
        );
        for (dump_id, all) in [(None, false), (Some("019a".to_owned()), true)] {
            assert_eq!(
                kind(dump(
                    "u",
                    DumpRequest::Stop { dump_id, all },
                    &u.gateway,
                    now
                )),
                "bad_request"
            );
        }
    }
}
