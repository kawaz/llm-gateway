//! llm-gateway のコマンドライン。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use llm_gateway::config::CredentialSpec;
use llm_gateway::credential::file::FileStore;
use llm_gateway::credential::{CredentialId, Kind, Persistence as _, oauth};
use llm_gateway::{Config, Gateway};

const USAGE: &str = "\
llm-gateway — クライアントに認証を意識させない、薄い LLM proxy

使い方:
  llm-gateway <コマンド> [オプション]
  llm-gateway login --type <種別> <名前>

コマンド:
  serve       待ち受けを始める
  check       設定を読んで確かめる (起動はしない)
  models      設定に書かれているモデルを一覧する
  login       ブラウザで認可を通し、認証情報を <名前>.json に保存する

オプション:
  --config <path>   設定ファイル (既定: $XDG_CONFIG_HOME/llm-gateway/config.toml)
  --help, -h        この説明
  --version         版を表示

login のオプション:
  --type <種別>     claude_oauth または codex_oauth
                    (config.toml の [credentials.<名前>] に書く type と同じ語)

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
    let rest = &args[1..];

    match command {
        "serve" => serve(&parse_config_path(rest)?),
        "check" => check(&parse_config_path(rest)?),
        "models" => models(&parse_config_path(rest)?),
        "login" => login(rest),
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

/// `login` に渡された内容。
#[derive(Debug, PartialEq, Eq)]
struct LoginArgs {
    name: String,
    kind: Kind,
    config_path: PathBuf,
}

/// ブラウザで認可を通し、認証情報を保存する。
fn login(args: &[String]) -> Result<ExitCode, String> {
    let LoginArgs {
        name,
        kind,
        config_path,
    } = parse_login_args(args)?;
    let config = load(&config_path)?;
    let declared = config.credentials.get(&name);
    check_declared_type(&name, kind, declared)?;

    let dir = config.store.resolve_dir();
    let id = CredentialId::new(&name);

    let runtime =
        tokio::runtime::Runtime::new().map_err(|e| format!("ランタイムを作れません: {e}"))?;

    runtime.block_on(async move {
        let store = FileStore::open(&dir).map_err(|e| e.to_string())?;

        // 受け口を開いてから URL を出す。出してから開くまでの間に戻ってこられても
        // 取りこぼさない。
        let authorization = oauth::begin(kind).await.map_err(|e| e.to_string())?;

        println!("次の URL をブラウザで開き、許可まで進めてください:");
        println!();
        println!("  {}", authorization.url());
        println!();
        open_browser(authorization.url());

        let tokens = authorization.finish().await.map_err(|e| e.to_string())?;

        // 既にあれば、その内容を土台にする (priority や除外リストを消さない)。
        let existing = store.load(&id).ok();
        let credential = tokens.to_stored(kind, existing.as_ref());
        store.store(&id, &credential).map_err(|e| e.to_string())?;

        println!(
            "{} に保存しました",
            dir.join(format!("{name}.json")).display()
        );
        if !credential.email.is_empty() {
            println!("アカウント {}", credential.email);
        }
        if declared.is_none() {
            print_config_hint(&name, kind);
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn parse_login_args(args: &[String]) -> Result<LoginArgs, String> {
    let mut name: Option<String> = None;
    let mut type_name: Option<String> = None;
    let mut config_path: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let Some(rest) = arg.strip_prefix("--") else {
            if let Some(previous) = name.replace(arg.clone()) {
                return Err(format!(
                    "名前が 2 つ指定されています (`{previous}` と `{arg}`)。名前は 1 つだけです"
                ));
            }
            continue;
        };
        let (key, inline) = match rest.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (rest, None),
        };
        match key {
            "type" => type_name = Some(take_value(key, inline, &mut it)?),
            "config" => config_path = Some(PathBuf::from(take_value(key, inline, &mut it)?)),
            other => return Err(format!("`--{other}` は解釈できません")),
        }
    }

    let name = name.ok_or(
        "認証情報の名前が指定されていません。\
`llm-gateway login --type claude_oauth <名前>` の形で指定します",
    )?;
    check_name(&name)?;

    let type_name =
        type_name.ok_or("--type が指定されていません。claude_oauth か codex_oauth を指定します")?;
    let kind = Kind::from_config_type(&type_name).ok_or_else(|| {
        format!("`{type_name}` には login できません。claude_oauth か codex_oauth を指定します")
    })?;

    Ok(LoginArgs {
        name,
        kind,
        config_path: config_path.unwrap_or_else(Config::default_path),
    })
}

fn take_value(
    key: &str,
    inline: Option<&str>,
    it: &mut std::slice::Iter<'_, String>,
) -> Result<String, String> {
    match inline {
        Some(value) => Ok(value.to_owned()),
        None => it
            .next()
            .cloned()
            .ok_or_else(|| format!("--{key} に値が指定されていません")),
    }
}

/// 名前はそのままファイル名になる。置き場の外に書けてしまう形を弾く。
///
/// `-` 始まりも弾く。綴りを間違えたオプションが名前として通ると、意図しない
/// ファイルに保存される。
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.starts_with('-')
    {
        return Err(format!(
            "`{name}` は認証情報の名前に使えません。\
名前はそのまま <名前>.json というファイル名になります"
        ));
    }
    Ok(())
}

/// 設定に宣言があるなら、種別が食い違っていないか見る。
///
/// 取り違えたまま保存すると、gateway が別の作法で使おうとして 401 になる。
/// 原因が認証側にあると気づきにくいので、保存する前に止める。
fn check_declared_type(
    name: &str,
    kind: Kind,
    declared: Option<&CredentialSpec>,
) -> Result<(), String> {
    // 宣言が無くても保存はする。先に認証情報を取ってから config.toml を書く
    // 順でも困らないようにしておく (書き方は保存後に案内する)。
    let Some(spec) = declared else {
        return Ok(());
    };

    match login_kind_of(spec) {
        Some(declared_kind) if declared_kind == kind => Ok(()),
        Some(declared_kind) => Err(format!(
            "`{name}` は config.toml では type = \"{t}\" で宣言されています。\
--type {t} を指定するか、別の名前を使ってください",
            t = declared_kind.config_type()
        )),
        None => Err(format!(
            "`{name}` は config.toml では login を要さない種別で宣言されています。\
login して使うのは claude_oauth と codex_oauth だけです"
        )),
    }
}

/// 設定の宣言に対応する login の種別。login できない種別は `None`。
fn login_kind_of(spec: &CredentialSpec) -> Option<Kind> {
    match spec {
        CredentialSpec::ClaudeOauth { .. } => Some(Kind::Claude),
        CredentialSpec::CodexOauth { .. } => Some(Kind::Codex),
        CredentialSpec::ClaudeBedrock { .. } | CredentialSpec::Relay { .. } => None,
    }
}

/// 保存はできたが設定に宣言が無い状態。次に何を書けばよいか示す。
fn print_config_hint(name: &str, kind: Kind) {
    println!();
    println!("`{name}` は config.toml の [credentials] にありません。次を足すと使えます:");
    println!();
    println!("  [credentials.{name}]");
    println!("  type = \"{}\"", kind.config_type());
}

/// ブラウザを開く。開けなくても止めない (URL は既に出してある)。
fn open_browser(url: &str) {
    let outcome = std::process::Command::new("open").arg(url).status();
    let reason = match outcome {
        Ok(status) if status.success() => return,
        Ok(status) => format!("open が {status} で終わりました"),
        Err(e) => e.to_string(),
    };
    eprintln!("ブラウザを開けませんでした ({reason})。上の URL を自分で開いてください");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn parse(list: &[&str]) -> Result<LoginArgs, String> {
        parse_login_args(&args(list))
    }

    #[test]
    fn login_takes_a_type_and_a_name() {
        let got = parse(&["--type", "claude_oauth", "claude-main"]).unwrap();
        assert_eq!(got.name, "claude-main");
        assert_eq!(got.kind, Kind::Claude);
        assert_eq!(got.config_path, Config::default_path());
    }

    /// オプションはメイン引数の後ろにも置ける。`=` 付きでも書ける。
    #[test]
    fn login_options_may_follow_the_name() {
        let separate = parse(&["codex-main", "--type", "codex_oauth"]).unwrap();
        let inline = parse(&["--type=codex_oauth", "codex-main"]).unwrap();

        assert_eq!(separate, inline);
        assert_eq!(separate.kind, Kind::Codex);
    }

    #[test]
    fn login_takes_a_config_path() {
        let got = parse(&["--type", "claude_oauth", "n", "--config", "/tmp/c.toml"]).unwrap();
        assert_eq!(got.config_path, PathBuf::from("/tmp/c.toml"));

        let inline = parse(&["--type", "claude_oauth", "n", "--config=/tmp/c.toml"]).unwrap();
        assert_eq!(inline.config_path, PathBuf::from("/tmp/c.toml"));
    }

    /// 何が足りないかを言う (help を読み直させない)。
    #[test]
    fn login_says_what_is_missing() {
        let err = parse(&["--type", "claude_oauth"]).unwrap_err();
        assert!(err.contains("名前"), "{err}");

        let err = parse(&["claude-main"]).unwrap_err();
        assert!(err.contains("--type"), "{err}");

        let err = parse(&["--type"]).unwrap_err();
        assert!(err.contains("--type に値"), "{err}");
    }

    /// login できない種別は、指定できる語を添えて断る。
    #[test]
    fn login_rejects_types_without_an_authorization_flow() {
        for bad in ["claude_bedrock", "relay", "claude", "oauth"] {
            let err = parse(&["--type", bad, "n"]).unwrap_err();
            assert!(err.contains("claude_oauth"), "{bad} → {err}");
        }
    }

    #[test]
    fn login_rejects_unknown_options() {
        let err = parse(&["--type", "claude_oauth", "n", "--nope"]).unwrap_err();
        assert!(err.contains("--nope"), "{err}");
    }

    /// 名前が 2 つあると、どちらに保存されるか分からない。黙って選ばない。
    #[test]
    fn login_rejects_two_names() {
        let err = parse(&["--type", "claude_oauth", "a", "b"]).unwrap_err();
        assert!(err.contains("1 つ"), "{err}");
        assert!(
            err.contains("`a`") && err.contains("`b`"),
            "両方を示す: {err}"
        );
    }

    /// 名前はそのままファイル名になる。置き場の外に書ける形を通さない。
    /// 綴りを誤ったオプション (`-type`) も名前として受け取らない。
    #[test]
    fn login_rejects_names_that_escape_the_store() {
        for bad in ["", ".", "..", "../../etc/passwd", "sub/name", "-type"] {
            let err = parse(&["--type", "claude_oauth", bad]).unwrap_err();
            assert!(
                err.contains("使えません") || err.contains("名前"),
                "{bad} → {err}"
            );
        }
    }

    fn claude_oauth() -> CredentialSpec {
        CredentialSpec::ClaudeOauth {
            url: "https://api.anthropic.com".to_owned(),
            headers: BTreeMap::new(),
            exclude: Vec::new(),
        }
    }

    fn relay() -> CredentialSpec {
        CredentialSpec::Relay {
            url: "http://127.0.0.1:8317".to_owned(),
            headers: BTreeMap::new(),
            models: Vec::new(),
            exclude: Vec::new(),
        }
    }

    /// 宣言が無い名前でも保存はできる (設定を書く前に取れる)。
    #[test]
    fn undeclared_name_is_allowed() {
        assert!(check_declared_type("new", Kind::Claude, None).is_ok());
    }

    #[test]
    fn declared_type_may_match() {
        assert!(check_declared_type("c", Kind::Claude, Some(&claude_oauth())).is_ok());
    }

    /// 種別を取り違えたまま保存すると、使う段になって 401 になる。手前で止める。
    #[test]
    fn declared_type_mismatch_is_refused() {
        let err = check_declared_type("c", Kind::Codex, Some(&claude_oauth())).unwrap_err();
        assert!(err.contains("claude_oauth"), "{err}");
    }

    #[test]
    fn declared_type_without_login_is_refused() {
        let err = check_declared_type("r", Kind::Claude, Some(&relay())).unwrap_err();
        assert!(err.contains("login"), "{err}");
    }

    /// login コマンドがある。無い名前は help へ案内する。
    #[test]
    fn unknown_command_points_at_help() {
        let err = run(&args(["logon"].as_ref())).unwrap_err();
        assert!(err.contains("--help"), "{err}");
    }

    /// help に login の使い方が載っている (実装と説明を揃える)。
    #[test]
    fn usage_documents_login() {
        assert!(USAGE.contains("login"), "{USAGE}");
        assert!(USAGE.contains("--type"), "{USAGE}");
        assert!(USAGE.contains("claude_oauth"), "{USAGE}");
        assert!(USAGE.contains("codex_oauth"), "{USAGE}");
    }
}
