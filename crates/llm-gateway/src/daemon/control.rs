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

/// 一時ダンプの頼みに答える (DR-0038 決定 1・10)。
///
/// 答えはどれも `unit` と `ok` で始まり、`ok: true` の答えには reference
/// `proxy-dump-design` の欄を同じ階層に添える。断る時は `ok: false` と
/// `error: {kind, message}`。`follow` の頼みはここでは受けない (接続を保つ口で
/// 受ける)。
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
            timeout_ms,
            max_bytes,
            store,
            labels,
            follow,
            embed,
            persist,
        } => {
            if follow || embed.is_some() || persist.is_some() {
                return dump_failed(
                    unit,
                    "bad_request",
                    "--embed and --no-persist need --follow, which this request did not ask for",
                );
            }
            // 走っている設定に無い ns は書く相手が無い (DR-0038 決定 2)。
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
                timeout_ms: timeout_ms.map(|ms| i64::try_from(ms).unwrap_or(i64::MAX)),
                max_bytes,
                store: store.as_deref(),
                labels,
            };
            dumps
                .start(&start, &gateway.dump_config(), now)
                .map(|started| fields(&started))
        }
        DumpRequest::Stop { dump_id } => dumps
            .stop(unit, &dump_id, now)
            .map(|listed| fields(&listed)),
        DumpRequest::List { selectors } => dumps.list(unit, &selectors, now).map(|listed| {
            let mut map = serde_json::Map::new();
            map.insert("dumps".to_owned(), serde_json::json!(listed));
            map
        }),
        DumpRequest::Prune {
            dump_ids,
            selectors,
        } => dumps.prune(unit, &dump_ids, &selectors, now).map(|pruned| {
            let mut map = serde_json::Map::new();
            map.insert("pruned".to_owned(), serde_json::json!(pruned));
            map
        }),
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

/// 構造体の欄を答えの欄にする。
fn fields(value: &impl serde::Serialize) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
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
            timeout_ms: Some(600_000),
            max_bytes: None,
            store: None,
            labels: std::collections::BTreeMap::from([("owner".to_owned(), "kawaz".to_owned())]),
            follow: false,
            embed: None,
            persist: None,
        }
    }

    /// 答えは `unit` と `ok` で始まり、reference の欄を同じ階層に添える
    /// (DR-0038 決定 10)。
    #[tokio::test]
    async fn a_dump_is_started_listed_stopped_and_pruned_through_the_control_socket() {
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
        assert_eq!(started["ok"], true, "{started}");
        assert_eq!(started["ns"], "default");
        assert_eq!(started["timeout_ms"], 600_000);
        assert_eq!(started["max_bytes"], 200 * 1024 * 1024);
        assert_eq!(started["store"], "raw");
        assert_eq!(started["labels"]["owner"], "kawaz");
        assert_eq!(started["concurrency"]["limit"], 16);
        assert!(
            started["storage"]["fs_free_bytes"].as_u64().is_some(),
            "{started}"
        );
        let id = started["dump_id"].as_str().unwrap().to_owned();
        let dir = started["dump_dir"].as_str().unwrap();
        assert!(
            dir.ends_with(&format!("stats/dumps/u/default/{id}")),
            "{dir}"
        );

        let listed = ask(DumpRequest::List {
            selectors: vec!["state=active".to_owned()],
        })
        .await;
        assert_eq!(listed["ok"], true);
        assert_eq!(listed["dumps"][0]["dump_id"], id.as_str());
        assert_eq!(listed["dumps"][0]["state"], "active");

        let stopped = ask(DumpRequest::Stop {
            dump_id: id.clone(),
        })
        .await;
        assert_eq!(stopped["ok"], true);
        assert_eq!(stopped["state"], "stopped");
        assert_eq!(stopped["reason"], "stop");
        assert!(stopped["stopped_at"].as_i64().is_some(), "{stopped}");
        let listed = ask(DumpRequest::List {
            selectors: vec!["state=active".to_owned()],
        })
        .await;
        assert_eq!(listed["dumps"], serde_json::json!([]));

        let pruned = ask(DumpRequest::Prune {
            dump_ids: vec![id.clone()],
            selectors: Vec::new(),
        })
        .await;
        assert_eq!(pruned["pruned"], serde_json::json!([id]));
        assert!(!Path::new(dir).exists());

        task.abort();
        let _ = task.await;
    }

    /// 走っている設定に無い ns、知らない id、指定の無い prune、follow の無い
    /// embed は断る。
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
                    dump_id: "019a".to_owned(),
                },
                &u.gateway,
                now
            )),
            "unknown_dump"
        );
        assert_eq!(
            kind(dump(
                "u",
                DumpRequest::Prune {
                    dump_ids: Vec::new(),
                    selectors: Vec::new(),
                },
                &u.gateway,
                now
            )),
            "bad_request"
        );
        let embedded = DumpRequest::Start {
            ns: "default".to_owned(),
            timeout_ms: None,
            max_bytes: None,
            store: None,
            labels: std::collections::BTreeMap::new(),
            follow: false,
            embed: Some("bodies".to_owned()),
            persist: None,
        };
        assert_eq!(kind(dump("u", embedded, &u.gateway, now)), "bad_request");
    }
}
