//! 登録された 1 台を、この端末の前で走らせる (DR-0028 決定 1)。
//!
//! 設定を解釈するのはここだけ。監督者は `<binary_path> daemon run <unit>` を
//! 子として起こすだけで、設定の中身を知らない。

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use llm_gateway::credential::file::FileStore;
use llm_gateway::daemon::registry::Registry;
use llm_gateway::{Config, Gateway};

use crate::failure::Failure;
use crate::help;

/// 使用量の集計と利用状況をディスクへ落とす間隔。
///
/// リクエストごとに書くのは無駄なので間隔を空ける (DR-0011)。落ちたときに
/// 失うのはこの間に通った分だけで、終了の合図では待たずに落とす。
const SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

pub fn foreground(registry: &Registry, args: &[String]) -> Result<ExitCode, Failure> {
    let Some(name) = args.first() else {
        // 必須の引数があるコマンドは、引数なしなら help を出す。
        print!("{}", help::DAEMON);
        return Ok(ExitCode::SUCCESS);
    };
    if let Some(unexpected) = args.get(1) {
        return Err(Failure::from(format!(
            "could not understand `{unexpected}`"
        )));
    }

    let unit = registry.get(name)?;
    serve(name, &unit.config)
}

fn serve(unit: &str, config_path: &Path) -> Result<ExitCode, Failure> {
    init_logging();
    let config = Config::load(config_path).map_err(|e| {
        Failure::new(
            "unit_config_unreadable",
            format!(
                "could not read the configuration of `{unit}` ({}): {e}",
                config_path.display()
            ),
        )
    })?;

    // 待ち受けないと書いてある設定で起動しようとしたら、その場で言う。
    // 黙って何もせずに終わると、起動したつもりのまま繋がらない原因を
    // 探すことになる。
    if config.server.disabled {
        return Err(Failure::new(
            "unit_disabled",
            format!(
                "`{unit}` reads {}, which has disabled = true (it does not listen). \
remove disabled from [server], or register another configuration",
                config_path.display()
            ),
        ));
    }

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))?;

    runtime.block_on(async move {
        let dir = config.store.resolve_dir();
        let store = FileStore::open(&dir).map_err(|e| Failure::from(e.to_string()))?;
        let gateway =
            Arc::new(Gateway::new(&config, store).map_err(|e| Failure::from(e.to_string()))?);

        let listener = tokio::net::TcpListener::bind(&config.server.listen)
            .await
            .map_err(|e| {
                Failure::from(format!("could not listen on {}: {e}", config.server.listen))
            })?;

        // 待ち受ける前に一覧を揃える。空の状態で受けると 404 を返してしまう。
        gateway.refresh_models().await;

        // 前回落とした分 (当日の集計・credential ごとの利用状況) を読み戻す。
        gateway
            .restore(llm_gateway::credential::time::now_unix())
            .await;
        gateway.start_status();
        gateway.start_keepalive();

        tracing::info!(
            unit,
            listen = %config.server.listen,
            credentials = %dir.display(),
            stats = %config.stats.resolve_dir().display(),
            namespaces = gateway.namespace_names().len(),
            "listening"
        );

        // 誰でも通る面は、起動時に名前を出す。手前で境界を引く運用では正しい
        // 姿だが、そのつもりが無いまま開いているのが一番危ない。
        let open: Vec<&str> = gateway
            .namespace_names()
            .into_iter()
            .filter(|name| {
                gateway
                    .namespace(name)
                    .is_some_and(|ns| ns.auth_token.is_none())
            })
            .collect();
        if !open.is_empty() {
            tracing::info!(
                namespaces = %open.join(", "),
                "exposed without authentication (put a boundary in front of it)"
            );
        }

        // 新しいモデルが出たときに、再起動せずに拾えるようにする。
        let refresher = Arc::clone(&gateway);
        tokio::spawn(async move { refresher.keep_models_fresh().await });

        // 集計と利用状況を定期的に落とす。
        let flusher = Arc::clone(&gateway);
        let flushing = tokio::spawn(async move { flusher.keep_saving(SAVE_INTERVAL).await });

        // 送り先があるときだけ購読する。Receiver は spawn より先に作り、
        // 待ち受け開始直後のイベントも取りこぼさない。
        let webhook = (!config.webhook.destinations().0.is_empty()).then(|| {
            let watching = gateway.events().subscribe();
            let webhook_config = config.webhook.clone();
            tokio::spawn(async move {
                llm_gateway::webhook::keep_sending(
                    webhook_config,
                    reqwest::Client::new(),
                    watching,
                )
                .await;
            })
        });

        let serving = Arc::clone(&gateway);
        let result = axum::serve(
            listener,
            llm_gateway_server::router(serving)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| Failure::from(format!("the server stopped listening: {e}")));

        // 定期の保存を先に止めて、終わるのを待つ。待たずに最後の保存へ進むと
        // 2 者が同時に書きうる。書き込み自体も直列化されているが、待つ側で
        // 重なりを消しておけば「同時に書いたが壊れなかった」に頼らずに済む。
        flushing.abort();
        let _ = flushing.await;
        if let Some(webhook) = webhook {
            webhook.abort();
            let _ = webhook.await;
        }

        // 止まる前に落とす。定期の周回を待たずに書くので、終了の合図で
        // 直前の分を失わない。
        gateway.save().await;
        result?;

        Ok(ExitCode::SUCCESS)
    })
}

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter =
        EnvFilter::try_from_env("LLM_GATEWAY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

/// 止められたときに、流している応答を切らずに終わる。
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "cannot receive SIGTERM");
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "cannot receive SIGINT");
            return;
        }
    };

    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = int.recv() => tracing::info!("received SIGINT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_gateway::daemon::registry::Unit;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn register(registry: &Registry, dir: &Path, name: &str, body: &str) {
        let config = dir.join(format!("{name}.toml"));
        std::fs::write(&config, body).unwrap();
        registry
            .add(
                name,
                &Unit {
                    config,
                    binary_path: std::path::PathBuf::from("/usr/local/bin/llm-gateway"),
                    enabled: true,
                    added_at: "2026-09-09T00:00:00Z".to_owned(),
                },
            )
            .unwrap();
    }

    /// 走らせる先は登録簿から引く。知らない名前は、あるものを添えて断る。
    #[test]
    fn running_an_unregistered_unit_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));

        let e = foreground(&registry, &args(&["nope"])).unwrap_err();
        assert_eq!(e.kind(), "unknown_unit");
    }

    /// 待ち受けない設定を登録したまま走らせたら、その場で断る。
    ///
    /// 黙って何もせずに終わると、起動したつもりのまま「繋がらない」原因を
    /// 探すことになる。
    #[test]
    fn running_a_disabled_configuration_stops_with_a_reason() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        register(
            &registry,
            dir.path(),
            "quiet",
            "[server]\ndisabled = true\nlisten = \"127.0.0.1:11300\"\n",
        );

        let e = foreground(&registry, &args(&["quiet"])).unwrap_err();
        assert_eq!(e.kind(), "unit_disabled");
        assert!(e.message().contains("quiet"), "names the unit: {e:?}");
        assert!(e.message().contains("disabled"), "{e:?}");
        assert!(e.message().contains("quiet.toml"), "names the file: {e:?}");
    }

    /// 余分な引数を黙って捨てない (打ち間違えた名前で別の台が走る)。
    #[test]
    fn a_second_argument_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));

        let e = foreground(&registry, &args(&["a", "b"])).unwrap_err();
        assert!(e.message().contains("`b`"), "{e:?}");
    }
}
