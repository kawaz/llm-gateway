//! llm-gateway のコマンドライン。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use llm_gateway::credential::file::FileStore;
use llm_gateway::credential::{CredentialId, Persistence as _};
use llm_gateway::{Config, Gateway};

const USAGE: &str = "\
llm-gateway — クライアントに認証を意識させない、薄い LLM proxy

使い方:
  llm-gateway <コマンド> [オプション]

コマンド:
  serve       待ち受けを始める
  check       設定を読んで確かめる (起動はしない)
  models      設定に書かれているモデルを一覧する

オプション:
  --config <path>   設定ファイル (既定: $XDG_CONFIG_HOME/llm-gateway/config.toml)
  --help, -h        この説明
  --version         版を表示

環境変数:
  LLM_GATEWAY_LOG   ログの詳しさ (既定: info)
  XDG_CONFIG_HOME   設定の既定の置き場
  XDG_STATE_HOME    認証情報とログの既定の置き場
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match run(&args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if args.iter().any(|a| a == "--version") {
        println!("llm-gateway {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    let command = args[0].as_str();
    let config_path = parse_config_path(&args[1..])?;

    match command {
        "serve" => serve(&config_path),
        "check" => check(&config_path),
        "models" => models(&config_path),
        other => Err(format!(
            "`{other}` というコマンドはありません。`llm-gateway --help` を見てください"
        )),
    }
}

fn parse_config_path(args: &[String]) -> Result<PathBuf, String> {
    let mut it = args.iter();
    let Some(arg) = it.next() else {
        return Ok(Config::default_path());
    };
    match arg.as_str() {
        "--config" => it
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| "--config にパスが指定されていません".to_owned()),
        other => match other.strip_prefix("--config=") {
            Some(path) => Ok(PathBuf::from(path)),
            None => Err(format!("`{other}` は解釈できません")),
        },
    }
}

fn load(path: &Path) -> Result<Config, String> {
    Config::load(path).map_err(|e| e.to_string())
}

fn serve(config_path: &Path) -> Result<ExitCode, String> {
    init_logging();
    let config = load(config_path)?;

    let runtime =
        tokio::runtime::Runtime::new().map_err(|e| format!("ランタイムを作れません: {e}"))?;

    runtime.block_on(async move {
        let dir = config.store.resolve_dir();
        let store = FileStore::open(&dir).map_err(|e| e.to_string())?;
        let gateway = Arc::new(Gateway::new(&config, store).map_err(|e| e.to_string())?);

        let listener = tokio::net::TcpListener::bind(&config.server.listen)
            .await
            .map_err(|e| format!("{} で待ち受けられません: {e}", config.server.listen))?;

        // 待ち受ける前に一覧を揃える。空の状態で受けると 404 を返してしまう。
        gateway.refresh_models().await;

        tracing::info!(
            listen = %config.server.listen,
            credentials = %dir.display(),
            namespaces = gateway.namespace_names().len(),
            "待ち受けを始めます"
        );

        // 新しいモデルが出たときに、再起動せずに拾えるようにする。
        let refresher = Arc::clone(&gateway);
        tokio::spawn(async move { refresher.keep_models_fresh().await });

        axum::serve(listener, llm_gateway_server::router(gateway))
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| format!("待ち受けが止まりました: {e}"))?;

        Ok(ExitCode::SUCCESS)
    })
}

/// 設定を読んで確かめるだけ。起動前の確認に使う。
fn check(config_path: &Path) -> Result<ExitCode, String> {
    let config = load(config_path)?;
    let dir = config.store.resolve_dir();

    println!("設定       {}", config_path.display());
    println!("待ち受け   {}", config.server.listen);
    println!("認証情報   {}", dir.display());
    println!("認証情報数 {} 件", config.credentials.len());
    println!("namespace  {}", config.namespace_names().join(", "));

    // 認証情報が置かれているかは、起動しないと分からない部分。ここで見ておくと
    // 動かしてから 401 で気づく事態を減らせる。
    let store = FileStore::open(&dir).map_err(|e| e.to_string())?;
    let missing: Vec<&str> = config
        .credentials
        .iter()
        .filter(|(name, spec)| {
            spec.needs_secret() && store.load(&CredentialId::new(*name)).is_err()
        })
        .map(|(name, _)| name.as_str())
        .collect();

    if missing.is_empty() {
        println!("\n問題ありません");
        return Ok(ExitCode::SUCCESS);
    }

    println!("\n次の認証情報が {} にありません:", dir.display());
    for name in &missing {
        println!("  {name}.json");
    }
    Ok(ExitCode::FAILURE)
}

/// upstream に聞いて、実際に公開されるモデルを出す。
fn models(config_path: &Path) -> Result<ExitCode, String> {
    let config = load(config_path)?;

    let runtime =
        tokio::runtime::Runtime::new().map_err(|e| format!("ランタイムを作れません: {e}"))?;

    runtime.block_on(async move {
        let store = FileStore::open(config.store.resolve_dir()).map_err(|e| e.to_string())?;
        let gateway = Gateway::new(&config, store).map_err(|e| e.to_string())?;
        gateway.refresh_models().await;

        let names: Vec<String> = gateway
            .namespace_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut any = false;

        for (i, ns_name) in names.iter().enumerate() {
            let Some(ns) = gateway.namespace(ns_name) else {
                continue;
            };
            let models = gateway.models(ns).await;
            if models.is_empty() {
                continue;
            }
            any = true;

            // namespace が 1 つだけなら見出しは邪魔。
            if names.len() > 1 {
                if i > 0 {
                    println!();
                }
                println!("[{ns_name}]");
            }
            for model in &models {
                let route = gateway.route_names(ns, model).await.join(" → ");
                println!("{model}\t{route}");
            }
        }

        if !any {
            println!("公開できるモデルがありません。");
            println!("認証情報が置かれているか、exclude で全部隠していないか確認してください。");
            return Ok(ExitCode::FAILURE);
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter =
        EnvFilter::try_from_env("LLM_GATEWAY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}

/// 止められたときに、流している応答を切らずに終わる。
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "SIGTERM を受け取れません");
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "SIGINT を受け取れません");
            return;
        }
    };

    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM を受け取りました"),
        _ = int.recv() => tracing::info!("SIGINT を受け取りました"),
    }
}
