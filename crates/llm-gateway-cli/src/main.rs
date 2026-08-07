//! llm-gateway のコマンドライン。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use llm_gateway::config::CredentialSpec;
use llm_gateway::credential::file::FileStore;
use llm_gateway::credential::{CredentialId, Kind, Persistence, StoredCredential, oauth};
use llm_gateway::metering::TokenKind;
use llm_gateway::quota::{CredentialUsage, Report, Window};
use llm_gateway::stats::{Counters, Report as StatsReport};
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
  usage       認証情報ごとの利用量を一覧する (server に問い合わせる)
  stats       list token usage and USD cost per credential x model x day
  login       ブラウザで認可を通し、認証情報を <名前>.json に保存する

オプション:
  --config <path>   設定ファイル (既定: $XDG_CONFIG_HOME/llm-gateway/config.toml)
  --help, -h        この説明
  --version         版を表示

usage のオプション:
  --refresh         使っていない認証情報にも最小のリクエストを投げて取り直す
                    (確認そのものが利用量を少し消費する)

stats options:
  --days <N>        show the last N days (default: 7, 0 for everything)

login のオプション:
  --type <種別>     claude_oauth または codex_oauth
                    (config.toml の [credentials.<名前>] に書く type と同じ語)

環境変数:
  LLM_GATEWAY_LOG   ログの詳しさ (既定: info)
  XDG_CONFIG_HOME   設定の既定の置き場
  XDG_STATE_HOME    認証情報とログの既定の置き場
";

/// 使用量の集計と利用状況をディスクへ落とす間隔。
///
/// リクエストごとに書くのは無駄なので間隔を空ける (DR-0011)。落ちたときに
/// 失うのはこの間に通った分だけで、終了の合図では待たずに落とす。
const SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

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
        "usage" => usage(rest),
        "stats" => stats(rest),
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

    // 待ち受けないと書いてある設定で起動しようとしたら、その場で言う。
    // 黙って何もせずに終わると、起動したつもりのまま繋がらない原因を
    // 探すことになる。
    if config.server.disabled {
        return Err(format!(
            "{} は disabled = true です (この設定では待ち受けません)。\
待ち受けたいなら [server] の disabled を外すか、別の設定を --config で指定してください",
            config_path.display()
        ));
    }

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

        // 前回落とした分 (当日の集計・credential ごとの利用状況) を読み戻す。
        gateway
            .restore(llm_gateway::credential::time::now_unix())
            .await;

        tracing::info!(
            listen = %config.server.listen,
            credentials = %dir.display(),
            stats = %config.stats.resolve_dir().display(),
            namespaces = gateway.namespace_names().len(),
            "待ち受けを始めます"
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
                "認証なしで公開しています (手前で境界を引いてください)"
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
        let webhook = config.webhook.base_url.as_ref().map(|_| {
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
        let result = axum::serve(listener, llm_gateway_server::router(serving))
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| format!("待ち受けが止まりました: {e}"));

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

/// 設定を読んで確かめるだけ。起動前の確認に使う。
fn check(config_path: &Path) -> Result<ExitCode, String> {
    let config = load(config_path)?;
    let dir = config.store.resolve_dir();

    println!("設定       {}", config_path.display());
    println!("待ち受け   {}", listen_line(&config.server));
    println!("認証情報   {}", dir.display());
    println!("認証情報数 {} 件", config.credentials.len());
    println!("namespace  {}", config.namespace_names().join(", "));

    // 認証情報が置かれているかは、起動しないと分からない部分。ここで見ておくと
    // 動かしてから 401 で気づく事態を減らせる。
    let store = FileStore::open(&dir).map_err(|e| e.to_string())?;
    let placed = store.list().unwrap_or_default();

    let mut missing = Vec::new();
    let mut unreadable = Vec::new();
    for name in config.credentials.keys() {
        let id = CredentialId::new(name.as_str());
        match store.load(&id) {
            Ok(_) => {}
            // ファイルはあるのに読めない場合を「ありません」と言うと、
            // 置き直しても直らない原因を探すことになる。
            Err(e) if placed.contains(&id) => unreadable.push((name.as_str(), e.to_string())),
            Err(_) => missing.push(name.as_str()),
        }
    }

    if missing.is_empty() && unreadable.is_empty() {
        println!("\n問題ありません");
        return Ok(ExitCode::SUCCESS);
    }

    if !missing.is_empty() {
        println!("\n次の認証情報が {} にありません:", dir.display());
        for name in &missing {
            println!("  {name}.json");
        }
        println!("  llm-gateway login --type <種別> <名前> で取得できます");
    }
    if !unreadable.is_empty() {
        println!("\n次の認証情報を読めません:");
        for (name, reason) in &unreadable {
            println!("  {name}: {reason}");
        }
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

/// `usage` に渡された内容。
#[derive(Debug, PartialEq, Eq)]
struct UsageArgs {
    refresh: bool,
    config_path: PathBuf,
}

/// 認証情報ごとの利用量を出す。
///
/// スナップショットを持っているのは server なので、ここは HTTP で聞いて
/// 整形するだけ (DR-0007)。CLI 単独では何も答えられない。
fn usage(args: &[String]) -> Result<ExitCode, String> {
    let UsageArgs {
        refresh,
        config_path,
    } = parse_usage_args(args)?;
    let config = load(&config_path)?;
    let url = usage_url(&config.server.listen, refresh);

    let runtime =
        tokio::runtime::Runtime::new().map_err(|e| format!("ランタイムを作れません: {e}"))?;

    runtime.block_on(async move {
        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .map_err(|e| server_unreachable(&config.server.listen, &e.to_string()))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("応答を読めません: {e}"))?;
        if !status.is_success() {
            return Err(format!("server が {status} を返しました: {body}"));
        }

        let report: Report =
            serde_json::from_str(&body).map_err(|e| format!("応答を解釈できません: {e}"))?;
        print!("{}", render(&report));
        Ok(ExitCode::SUCCESS)
    })
}

fn parse_usage_args(args: &[String]) -> Result<UsageArgs, String> {
    let mut refresh = false;
    let mut config_path: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--refresh" => refresh = true,
            "--config" => {
                config_path = Some(PathBuf::from(
                    it.next()
                        .cloned()
                        .ok_or("--config にパスが指定されていません")?,
                ));
            }
            other => match other.strip_prefix("--config=") {
                Some(path) => config_path = Some(PathBuf::from(path)),
                None => return Err(format!("`{other}` は解釈できません")),
            },
        }
    }

    Ok(UsageArgs {
        refresh,
        config_path: config_path.unwrap_or_else(Config::default_path),
    })
}

/// `stats` に渡された内容。
#[derive(Debug, PartialEq, Eq)]
struct StatsArgs {
    days: usize,
    config_path: PathBuf,
}

/// 認証情報 × モデル × 日のトークン使用量を出す。
///
/// 集計を持っているのは server なので、ここは HTTP で聞いて整形するだけ
/// (usage と同じ形、DR-0011)。過去日の分もディスクに残っているので、
/// server が再集計せずに返す。
fn stats(args: &[String]) -> Result<ExitCode, String> {
    let StatsArgs { days, config_path } = parse_stats_args(args)?;
    let config = load(&config_path)?;
    let url = format!(
        "{}?days={days}",
        gateway_url(&config.server.listen, "stats")
    );

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("could not start the async runtime: {e}"))?;

    runtime.block_on(async move {
        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .map_err(|e| server_unreachable(&config.server.listen, &e.to_string()))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("could not read the response: {e}"))?;
        if !status.is_success() {
            return Err(format!("the server returned {status}: {body}"));
        }

        let report: StatsReport = serde_json::from_str(&body)
            .map_err(|e| format!("could not parse the response: {e}"))?;
        print!("{}", render_stats(&report));
        Ok(ExitCode::SUCCESS)
    })
}

fn parse_stats_args(args: &[String]) -> Result<StatsArgs, String> {
    let mut days = 7;
    let mut config_path: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--days" => {
                days = take_days(it.next().map(String::as_str))?;
            }
            "--config" => {
                config_path = Some(PathBuf::from(
                    it.next().cloned().ok_or("--config needs a path")?,
                ));
            }
            other => {
                if let Some(v) = other.strip_prefix("--days=") {
                    days = take_days(Some(v))?;
                } else if let Some(path) = other.strip_prefix("--config=") {
                    config_path = Some(PathBuf::from(path));
                } else {
                    return Err(format!("could not understand `{other}`"));
                }
            }
        }
    }

    Ok(StatsArgs {
        days,
        config_path: config_path.unwrap_or_else(Config::default_path),
    })
}

/// `--days` の値を読む。
///
/// 上限で抑えるのは、日数を秒に直す掛け算が桁あふれするため (`llm_gateway::stats`
/// の `MAX_DAYS`)。全期間を見たいなら `--days 0`。
fn take_days(value: Option<&str>) -> Result<usize, String> {
    let raw = value.ok_or("--days needs a number of days")?;
    raw.parse::<usize>()
        .map(|days| days.min(llm_gateway::stats::MAX_DAYS))
        .map_err(|_| format!("--days takes a whole number, got `{raw}`"))
}

/// 問い合わせ先。
///
/// 設定の `listen` は待ち受け側の書き方なので、どこからでも受ける指定
/// (`0.0.0.0` / `::`) をそのまま宛先にはできない。手元から叩く前提で
/// loopback に読み替える。
fn usage_url(listen: &str, refresh: bool) -> String {
    let query = if refresh { "?refresh=true" } else { "" };
    format!("{}{query}", gateway_url(listen, "usage"))
}

/// `/llm-gateway/<name>` の宛先。
fn gateway_url(listen: &str, name: &str) -> String {
    let host = match listen.rsplit_once(':') {
        Some((host, port)) => {
            let host = match host.trim_matches(['[', ']']) {
                "" | "0.0.0.0" | "::" => "127.0.0.1",
                other => other,
            };
            if other_is_ipv6(host) {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            }
        }
        None => listen.to_owned(),
    };
    format!("http://{host}/llm-gateway/{name}")
}

fn other_is_ipv6(host: &str) -> bool {
    host.contains(':')
}

fn server_unreachable(listen: &str, reason: &str) -> String {
    format!(
        "{listen} の server に繋がりません ({reason})。\n\
         起動しているか確認してください (launchctl list | grep llm-gateway、または just status)"
    )
}

/// 人が読む形に整える。claude-statusline の 5h/7d バー表示 (`dualBar` /
/// `dualInfo`) を Rust に移植し、1 credential 1 行で並べる。
fn render(report: &Report) -> String {
    let now = report.generated_at;
    let color = color_enabled();
    let name_width = report
        .credentials
        .iter()
        .map(|c| width(&c.name))
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for c in &report.credentials {
        if !out.is_empty() {
            // バーの ▀ が上下の行と溶けて見えるので、1 行ずつ空ける (kawaz 裁定)。
            out.push_str("\n\n");
        }
        out.push_str(&c.name);
        out.push_str(&" ".repeat(name_width.saturating_sub(width(&c.name)) + 2));

        match &c.snapshot {
            Some(s) => {
                out.push_str(&window_field(
                    '⏰',
                    s.five_hour.as_ref(),
                    now,
                    5 * 3600,
                    color,
                ));
                out.push(' ');
                out.push_str(&window_field(
                    '📆',
                    s.seven_day.as_ref(),
                    now,
                    7 * 86_400,
                    color,
                ));
                let age = now - s.observed_at;
                if age > 300 {
                    out.push_str(&format!(" ({})", elapsed(age)));
                }
            }
            None => out.push_str(match c.support {
                llm_gateway::quota::Support::Unobserved => "unobserved",
                llm_gateway::quota::Support::NotApplicable => "not_applicable",
                llm_gateway::quota::Support::UpstreamDependent => "upstream_dependent",
                llm_gateway::quota::Support::Observed => "-",
            }),
        }
    }

    // 上限や従量課金の状態は、行に収めると読み飛ばされる。当たっている
    // ものだけ下に並べる。
    for c in &report.credentials {
        for line in remarks(c) {
            out.push_str(&format!("\n{line}"));
        }
    }
    // probe の消費報告は出さない (kawaz 裁定: 要らない)。JSON には残っているので、
    // 消費量を確かめたければ /llm-gateway/usage?refresh=true を直接見る。
    out.push('\n');
    out
}

/// 日次集計を人が読む形に整える。
///
/// 1 つの桁組に全部を並べる。日ごとに合計を先に出し、その下に認証情報 ×
/// モデルの内訳を字下げして置く。合計を先にするのは「その日いくら使ったか」が
/// 一番よく見る数字で、内訳は当たりを付けた後に見るため。
///
/// 桁は全日で揃える。日ごとに幅が変わると、縦に並べて比べられない。
fn render_stats(report: &StatsReport) -> String {
    if report.days.is_empty() {
        return "No usage recorded yet.\n\
                Nothing has been forwarded through the gateway, \
                or the stats directory is empty.\n"
            .to_owned();
    }

    // 先に全行を組み立てる。幅は全体を見てからでないと決まらない。
    let mut lines: Vec<Line> = Vec::new();
    // 新しい日を上に出す。見たいのは直近。
    let mut all_days = Counters::default();
    for (date, day) in report.days.iter().rev() {
        let mut total = Counters::default();
        for models in day.credentials.values() {
            for entry in models.values() {
                total.merge(&entry.counters);
            }
        }
        all_days.merge(&total);
        lines.push((format!("{date} {TOTAL_LABEL}"), total, day.total_usd));

        for (cred, models) in &day.credentials {
            for (model, entry) in models {
                lines.push((
                    format!("  {cred} {model}"),
                    entry.counters.clone(),
                    entry.usd,
                ));
            }
        }
    }
    // 全期間の合計を最後に置く。日ごとの合計と同じ桁組に並ぶので、
    // 「今月いくら使ったか」を表の下端で読める。
    lines.push((TOTAL_LABEL.to_owned(), all_days, report.total_usd));

    // 列は記録に現れた区分だけ並べる。区分は provider ごとに違うので
    // (DR-0014 §4)、固定の列を並べると知らない区分が表から消える。
    let kinds = kinds_of(&lines);
    let headers: Vec<&str> = std::iter::once(REQUESTS_HEADER)
        .chain(kinds.iter().map(header_of))
        .collect();

    let label_width = lines.iter().map(|(l, ..)| width(l)).max().unwrap_or(0);
    let cell_width = lines
        .iter()
        .flat_map(|(_, c, _)| columns(c, &kinds))
        .map(|n| thousands(n).len())
        .max()
        .unwrap_or(0)
        .max(headers.iter().map(|h| h.len()).max().unwrap_or(0));
    let usd_width = lines
        .iter()
        .map(|(.., usd)| usd_cell(*usd).len())
        .chain(std::iter::once(USD_HEADER.len()))
        .max()
        .unwrap_or(0);

    // 見出しは 1 度だけ。日ごとに挟むと、行数の少ない日ほど見出しで埋まる。
    let mut out = " ".repeat(label_width);
    for head in &headers {
        out.push_str(&format!(" {head:>cell_width$}"));
    }
    out.push_str(&format!(" {USD_HEADER:>usd_width$}\n"));

    let mut previous_day_ended = false;
    for (label, counters, usd) in &lines {
        // 日の切り替わりで 1 行空ける。合計行は字下げが無いので見分けられる。
        if !label.starts_with(' ') && previous_day_ended {
            out.push('\n');
        }
        previous_day_ended = true;

        out.push_str(label);
        out.push_str(&" ".repeat(label_width.saturating_sub(width(label))));
        out.push_str(&row(counters, &kinds, cell_width));
        out.push_str(&format!(" {:>usd_width$}\n", usd_cell(*usd)));
    }
    out
}

/// 表示用の 1 行。`(ラベル, 集計, USD)`。
type Line = (String, Counters, Option<f64>);

/// USD の欄。単価表に無いモデルは `-`。
///
/// 小数 4 桁で止めるのは、1 リクエストが 0.1 セント未満になることがあり、
/// 2 桁だと内訳が全部 `0.00` に潰れるため。
fn usd_cell(usd: Option<f64>) -> String {
    usd.map_or_else(|| "-".to_owned(), |v| format!("{v:.4}"))
}

/// 合計行の印。内訳の行と見分けられればよい。
const TOTAL_LABEL: &str = "total";

/// 本数の見出し。トークンの区分より前に置く。
const REQUESTS_HEADER: &str = "reqs";

/// トークン数の右に置く金額の見出し。単位を書くのは、桁だけでは
/// トークン数と見分けが付かないため。
const USD_HEADER: &str = "usd";

/// 見慣れた並び。ここに挙げた区分を先にこの順で出し、残りは名前順で続ける。
///
/// 並べ替えの都合なので、ここに無い区分も落とさない (落とすと、その provider の
/// 消費が表から消える)。
const KIND_ORDER: [&str; 5] = [
    TokenKind::INPUT_NAME,
    TokenKind::OUTPUT_NAME,
    TokenKind::INPUT_CACHE_CREATION_NAME,
    TokenKind::INPUT_CACHE_READ_NAME,
    TokenKind::OUTPUT_REASONING_NAME,
];

/// 表に出す区分を、並べる順で返す。
fn kinds_of(lines: &[Line]) -> Vec<TokenKind> {
    let seen: BTreeSet<TokenKind> = lines
        .iter()
        .flat_map(|(_, c, _)| c.tokens.tokens.keys().cloned())
        .collect();

    let mut ordered: Vec<TokenKind> = KIND_ORDER
        .iter()
        .map(|name| TokenKind::new(*name))
        .filter(|kind| seen.contains(kind))
        .collect();
    ordered.extend(
        seen.into_iter()
            .filter(|kind| !KIND_ORDER.contains(&kind.as_str())),
    );
    ordered
}

/// 区分の見出し。桁が広がらないよう、標準区分は短い名前で出す。
///
/// 知らない区分は名前をそのまま見出しにする。短縮した名前を勝手に付けると、
/// どの区分を見ているのか読み手に分からない。
fn header_of(kind: &TokenKind) -> &str {
    match kind.as_str() {
        TokenKind::INPUT_CACHE_CREATION_NAME => "cache_w",
        TokenKind::INPUT_CACHE_READ_NAME => "cache_r",
        TokenKind::OUTPUT_REASONING_NAME => "reasoning",
        other => other,
    }
}

/// 1 行に並べる数。先頭は本数、続けて区分ごとのトークン数。
fn columns(c: &Counters, kinds: &[TokenKind]) -> Vec<u64> {
    std::iter::once(c.requests)
        .chain(kinds.iter().map(|kind| c.tokens.get(kind).unwrap_or(0)))
        .collect()
}

/// 数を桁揃えで 1 行に並べる。見出しと同じ幅で、先頭に区切りの 1 桁を置く。
fn row(c: &Counters, kinds: &[TokenKind], cell_width: usize) -> String {
    columns(c, kinds)
        .iter()
        .map(|n| format!(" {:>cell_width$}", thousands(*n)))
        .collect()
}

/// 3 桁ごとに区切る。トークン数は 6〜7 桁になるので、区切らないと読めない。
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 1 つの窓 (5h/7d) の表示。使用率とウィンドウ経過率を `dualBar` で重ねて
/// 描き、続けて `使用率%/経過率%/残り時間` を出す。取れていなければ `-`。
fn window_field(
    icon: char,
    window: Option<&Window>,
    now: i64,
    window_secs: i64,
    color: bool,
) -> String {
    let (Some(util), Some(reset)) = (
        window.and_then(|w| w.utilization),
        window.and_then(|w| w.reset),
    ) else {
        return format!("{icon}-");
    };
    let util_pct = util * 100.0;
    let elapsed_pct = calc_elapsed(reset, window_secs, now);
    let bar = dual_bar(util_pct, elapsed_pct, 10, color);
    // 7d 窓は日〜分をまたいで幅が揺れるので、先頭単位を 2 桁にして 6 桁固定にする。
    // 5h 窓は 10 時間に届かず 5 桁で揃うので、そのまま。
    let wide = window_secs >= 86_400;
    let remaining = format_duration(reset - now, wide);
    let info = dual_info(util_pct, elapsed_pct, &remaining, color);
    format!("{icon}{bar}{info}")
}

/// リセット時刻とウィンドウ長から、窓の経過率 (0〜100) を逆算する。
/// (claude-statusline `calcElapsed` の移植)
fn calc_elapsed(reset: i64, window_secs: i64, now: i64) -> f64 {
    if window_secs <= 0 {
        return 0.0;
    }
    let window_start = reset - window_secs;
    let pct = (now - window_start) as f64 / window_secs as f64 * 100.0;
    pct.clamp(0.0, 100.0)
}

/// 使用率に応じた危険度の色 (xterm-256)。(`utilColor` の移植)
fn util_color(util_pct: f64) -> u8 {
    if util_pct >= 80.0 {
        196
    } else if util_pct >= 50.0 {
        220
    } else {
        40
    }
}

/// 半ブロック `▀` を width 個並べ、上半分の色 (fg) で使用率、下半分の色 (bg)
/// でウィンドウ経過率を同時に表す。(`dualBar` の移植)
fn dual_bar(util_pct: f64, elapsed_pct: f64, width: usize, color: bool) -> String {
    let util_pct = util_pct.clamp(0.0, 100.0);
    let elapsed_pct = elapsed_pct.clamp(0.0, 100.0);
    let top_filled = (util_pct / 100.0 * width as f64).round() as usize;
    let bot_filled = (elapsed_pct / 100.0 * width as f64).round() as usize;

    let mut out = String::new();
    for i in 0..width {
        let fg = if i < top_filled {
            util_color(util_pct)
        } else {
            240
        };
        let bg = if i < bot_filled { 39 } else { 24 };
        out.push_str(&sgr_fg(fg, color));
        out.push_str(&sgr_bg(bg, color));
        out.push('▀');
    }
    out.push_str(&sgr_reset(color));
    out
}

/// `<使用率>%/<窓の経過率>%/<残り時間>`。(`dualInfo` の移植)
fn dual_info(util_pct: f64, elapsed_pct: f64, remaining: &str, color: bool) -> String {
    format!(
        "{}{:>2.0}%{}/{}{:>2.0}%{}/{}{}{}",
        sgr_fg(util_color(util_pct), color),
        util_pct,
        sgr_reset(color),
        sgr_fg(39, color),
        elapsed_pct,
        sgr_reset(color),
        sgr_fg(24, color),
        remaining,
        sgr_reset(color),
    )
}

/// 残り時間を `3h01m` / `1d20h` のような形式にする。(`formatDuration` の移植)
///
/// `wide` なら先頭の単位を 2 桁にして `01d20h` / `00h32m` の 6 桁固定にする。
/// 複数 credential を縦に並べる表で、日〜分をまたいでも桁が波打たないように。
fn format_duration(secs: i64, wide: bool) -> String {
    let total_m = secs.max(0) / 60;
    let m = total_m % 60;
    let total_h = total_m / 60;
    let h = total_h % 24;
    let d = total_h / 24;

    if d > 0 {
        if wide {
            format!("{d:02}d{h:02}h")
        } else {
            format!("{d}d{h:02}h")
        }
    } else if wide || total_h >= 10 {
        format!("{total_h:02}h{m:02}m")
    } else if total_h >= 1 {
        format!("{total_h}h{m:02}m")
    } else {
        // `32m` だと `1h01m` / `1d01h` と桁が揃わず表が波打つ。`0h` を付けて幅を保つ。
        format!("0h{m:02}m")
    }
}

/// `NO_COLOR` が空でない値で設定されているか。
fn no_color() -> bool {
    std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty())
}

/// 色を出してよいか (`NO_COLOR` 無指定 かつ 標準出力が TTY)。
fn color_enabled() -> bool {
    use std::io::IsTerminal as _;
    !no_color() && std::io::stdout().is_terminal()
}

fn sgr(params: &str, color: bool) -> String {
    if color {
        format!("\x1b[{params}m")
    } else {
        String::new()
    }
}

fn sgr_fg(n: u8, color: bool) -> String {
    sgr(&format!("38;5;{n}"), color)
}

fn sgr_bg(n: u8, color: bool) -> String {
    sgr(&format!("48;5;{n}"), color)
}

fn sgr_reset(color: bool) -> String {
    sgr("0", color)
}

fn elapsed(secs: i64) -> String {
    match secs {
        s if s < 60 => "たった今".to_owned(),
        s if s < 3600 => format!("{} 分前", s / 60),
        s if s < 86_400 => format!("{} 時間前", s / 3600),
        s => format!("{} 日前", s / 86_400),
    }
}

/// 表に収まらないもの (上限到達の警告・プローブの失敗)。
///
/// overage (従量課金フォールバックの可否) と window の status はここに出さない。
/// どちらも使用率の % を見れば足りる情報の重複で、毎回並ぶとノイズになる
/// (kawaz 裁定)。JSON には残っているので、機械で読む分には失われない。
fn remarks(c: &CredentialUsage) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(e) = &c.probe_error {
        lines.push(format!("{}: 取り直せませんでした ({e})", c.name));
    }
    // モデル別の枠は上の行に出せない (5 時間 / 7 日の欄しかない) うえ、
    // 応答ヘッダにも現れないので、聞けたときはここに並べる。
    for limit in c.limits.iter().flatten() {
        let Some(model) = &limit.model else {
            continue;
        };
        let mut line = format!("{}: {model} の枠は {:.0}%", c.name, limit.percent);
        if let Some(at) = &limit.resets_at {
            line.push_str(&format!(" ({} に開きます)", to_the_second(at)));
        }
        lines.push(line);
    }
    lines
}

/// 端数の秒を落とす。
///
/// `2026-08-02T08:59:59.571875+00:00` → `2026-08-02T08:59:59+00:00`。
/// 6 桁の端数まで読める人はいないので、行を短くする分だけ読みやすい。
fn to_the_second(at: &str) -> String {
    let Some(dot) = at.find('.') else {
        return at.to_owned();
    };
    let zone = at[dot..]
        .find(['+', '-', 'Z'])
        .map_or(at.len(), |offset| dot + offset);
    format!("{}{}", &at[..dot], &at[zone..])
}

/// 端末上の見た目の幅。日本語は 2 桁ぶん取る。
fn width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x20000..=0x3FFFD)
}

/// 認可で得た token を置き場に書く。
///
/// 土台を読んでから書くまでを締め出す (DR-0010)。再ログインするのは refresh
/// token が失効したときで、その裏では常駐している側が同じ認証情報の更新を
/// 試している。締め出さないと、認可の結果と相手の書き込みが互いを消し合う。
fn save<P: Persistence>(
    store: &P,
    id: &CredentialId,
    kind: Kind,
    tokens: &oauth::Tokens,
) -> llm_gateway::Result<StoredCredential> {
    let _guard = store.lock(id)?;
    // 既にあれば、その内容を土台にする (priority や除外リストを消さない)。
    let existing = store.load(id).ok();
    let credential = tokens.to_stored(kind, existing.as_ref());
    store.store(id, &credential)?;
    Ok(credential)
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
        println!("許可のあと、token を取得して使えることを確かめてから保存します。");
        println!("ブラウザの画面はその結果が出るまで待ちます。");
        open_browser(authorization.url());

        // 交換・確認・保存が全部済んでから、ブラウザにも端末にも結果が出る。
        let credential = authorization
            .finish(|tokens| save(&store, &id, kind, tokens))
            .await
            .map_err(|e| e.to_string())?;

        println!(
            "token が使えることを確認し、{} に保存しました",
            dir.join(format!("{name}.json")).display()
        );
        if !credential.payload.email().is_empty() {
            println!("アカウント {}", credential.payload.email());
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
/// `check` に出す待ち受け行。
///
/// 待ち受けない設定でも住所は出す。CLI (`usage` / `stats`) の問い合わせ先は
/// この値で組むので、伏せると「どこへ聞きに行くのか」が見えなくなる。
fn listen_line(server: &llm_gateway::config::Server) -> String {
    if server.disabled {
        return format!("{} (無効: この設定では待ち受けません)", server.listen);
    }
    server.listen.clone()
}

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
        CredentialSpec::ClaudeOauth => Some(Kind::Claude),
        CredentialSpec::CodexOauth => Some(Kind::Codex),
        CredentialSpec::BedrockApiKey => None,
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
    use std::sync::Mutex;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// 置き場に何をした順に、何をしたかを覚える偽物。
    #[derive(Default)]
    struct Recorder {
        steps: Arc<Mutex<Vec<&'static str>>>,
        current: Mutex<Option<StoredCredential>>,
    }

    /// 手放したことも記録に残す。締め出しっぱなしを見つけるため。
    struct Mark(Arc<Mutex<Vec<&'static str>>>);

    impl Drop for Mark {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("unlock");
        }
    }

    impl Recorder {
        fn holding(c: StoredCredential) -> Self {
            Self {
                steps: Arc::default(),
                current: Mutex::new(Some(c)),
            }
        }

        fn note(&self, step: &'static str) {
            self.steps.lock().unwrap().push(step);
        }

        fn steps(&self) -> Vec<&'static str> {
            self.steps.lock().unwrap().clone()
        }
    }

    impl Persistence for Recorder {
        type Guard = Mark;

        fn load(&self, _id: &CredentialId) -> llm_gateway::Result<StoredCredential> {
            self.note("load");
            self.current
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| llm_gateway::Error::Credential {
                    id: "c".to_owned(),
                    reason: "not stored yet".to_owned(),
                })
        }
        fn store(&self, _id: &CredentialId, v: &StoredCredential) -> llm_gateway::Result<()> {
            self.note("store");
            *self.current.lock().unwrap() = Some(v.clone());
            Ok(())
        }
        fn list(&self) -> llm_gateway::Result<Vec<CredentialId>> {
            Ok(vec![])
        }
        fn lock(&self, _id: &CredentialId) -> llm_gateway::Result<Self::Guard> {
            self.note("lock");
            Ok(Mark(Arc::clone(&self.steps)))
        }
        fn version(&self, _id: &CredentialId) -> Option<u64> {
            None
        }
    }

    fn fresh_tokens() -> oauth::Tokens {
        oauth::Tokens {
            access_token: "at-new".into(),
            refresh_token: "rt-new".into(),
            id_token: None,
            expires_in: 28_800,
            email: Some("someone@example.com".into()),
            account_id: None,
        }
    }

    /// 認可の結果を書くまでの間、置き場を締め出す。
    ///
    /// 再ログインは refresh token が失効したときに走るので、常駐している側が
    /// 同じ認証情報の更新を試している最中に当たりやすい。
    #[test]
    fn a_login_writes_under_the_lock() {
        let disk = Recorder::default();
        let id = CredentialId::new("c");

        save(&disk, &id, Kind::Claude, &fresh_tokens()).unwrap();

        assert_eq!(
            disk.steps(),
            vec!["lock", "load", "store", "unlock"],
            "土台を読む前に締め出し、書き終えてから手放す"
        );
    }

    /// 締め出している間に読み直すので、運用側の値を土台のまま引き継げる。
    #[test]
    fn a_login_keeps_what_the_operator_set() {
        let mut existing = StoredCredential::new(llm_gateway::credential::Payload::ClaudeOauth(
            llm_gateway::credential::OauthTokens {
                access_token: "at-old".into(),
                refresh_token: "rt-old".into(),
                expired: "2026-07-28T02:54:00+09:00".into(),
                email: "someone@example.com".into(),
                extra: Default::default(),
            },
        ));
        existing.priority = 10;
        existing.excluded_models = vec!["claude-opus-*".to_owned()];

        let disk = Recorder::holding(existing);
        let saved = save(
            &disk,
            &CredentialId::new("c"),
            Kind::Claude,
            &fresh_tokens(),
        )
        .unwrap();

        assert_eq!(saved.payload.secret(), "at-new");
        assert_eq!(saved.priority, 10);
        assert_eq!(saved.excluded_models, vec!["claude-opus-*"]);
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
        for bad in ["bedrock_api_key", "relay", "claude", "oauth"] {
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
        CredentialSpec::ClaudeOauth
    }

    fn without_login() -> CredentialSpec {
        CredentialSpec::BedrockApiKey
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
        let err = check_declared_type("r", Kind::Claude, Some(&without_login())).unwrap_err();
        assert!(err.contains("login"), "{err}");
    }

    fn usage_args(list: &[&str]) -> Result<UsageArgs, String> {
        parse_usage_args(&args(list))
    }

    #[test]
    fn usage_takes_refresh_and_config() {
        let plain = usage_args(&[]).unwrap();
        assert!(!plain.refresh, "既定では投げない (利用量を消費しない)");
        assert_eq!(plain.config_path, Config::default_path());

        let refreshing = usage_args(&["--refresh", "--config", "/tmp/c.toml"]).unwrap();
        assert!(refreshing.refresh);
        assert_eq!(refreshing.config_path, PathBuf::from("/tmp/c.toml"));

        assert_eq!(
            usage_args(&["--config=/tmp/c.toml"]).unwrap().config_path,
            PathBuf::from("/tmp/c.toml")
        );
    }

    #[test]
    fn usage_rejects_unknown_options() {
        let err = usage_args(&["--refesh"]).unwrap_err();
        assert!(err.contains("--refesh"), "{err}");
    }

    /// 待ち受けの書き方をそのまま宛先にしない。
    ///
    /// `0.0.0.0` は「どこからでも受ける」の意味で、繋ぎに行く先ではない。
    #[test]
    fn listen_address_becomes_a_reachable_url() {
        assert_eq!(
            usage_url("127.0.0.1:11300", false),
            "http://127.0.0.1:11300/llm-gateway/usage"
        );
        assert_eq!(
            usage_url("0.0.0.0:11300", false),
            "http://127.0.0.1:11300/llm-gateway/usage"
        );
        assert_eq!(
            usage_url("[::]:11300", false),
            "http://127.0.0.1:11300/llm-gateway/usage"
        );
        assert_eq!(
            usage_url("[::1]:11300", false),
            "http://[::1]:11300/llm-gateway/usage"
        );
        assert_eq!(
            usage_url("127.0.0.1:11300", true),
            "http://127.0.0.1:11300/llm-gateway/usage?refresh=true"
        );
    }

    /// 待ち受けない設定で `serve` したら、その場で断る。
    ///
    /// 黙って何もせずに終わると、起動したつもりのまま「繋がらない」原因を
    /// 探すことになる。
    #[test]
    fn serving_a_disabled_config_stops_with_a_reason() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("dummy.toml");
        std::fs::write(
            &path,
            "[server]\ndisabled = true\nlisten = \"127.0.0.1:11300\"\n",
        )
        .unwrap();

        let e = serve(&path).unwrap_err();
        assert!(e.contains("disabled"), "{e}");
        assert!(e.contains("dummy.toml"), "どの設定かが分かる: {e}");
        assert!(e.contains("--config"), "次にどうするかまで言う: {e}");
    }

    /// 待ち受けない設定でも `check` は通る。無効だとは明示する。
    #[test]
    fn checking_a_disabled_config_succeeds_and_says_so() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("dummy.toml");
        // 認証情報の置き場は試験用の空ディレクトリにする (実運用を触らない)。
        std::fs::write(
            &path,
            format!(
                "[server]\ndisabled = true\nlisten = \"127.0.0.1:11300\"\n\n\
                 [store]\ntype = \"file\"\ndir = \"{}\"\n",
                dir.path().join("creds").display()
            ),
        )
        .unwrap();

        assert_eq!(check(&path).unwrap(), ExitCode::SUCCESS);
    }

    /// 待ち受け行は、無効かどうかで書き分ける。住所そのものは伏せない。
    #[test]
    fn the_listen_line_says_when_it_does_not_listen() {
        use llm_gateway::config::Server;

        let listening = Server {
            listen: "127.0.0.1:11300".to_owned(),
            disabled: false,
        };
        assert_eq!(listen_line(&listening), "127.0.0.1:11300");

        let quiet = Server {
            disabled: true,
            ..listening
        };
        let line = listen_line(&quiet);
        assert!(
            line.contains("127.0.0.1:11300"),
            "問い合わせ先は見える: {line}"
        );
        assert!(line.contains("無効"), "{line}");
    }

    /// server が居ないときは、次に何を見ればよいかまで言う。
    #[test]
    fn unreachable_server_says_what_to_check() {
        let message = server_unreachable("127.0.0.1:11300", "connection refused");
        assert!(message.contains("127.0.0.1:11300"), "{message}");
        assert!(message.contains("起動"), "{message}");
        assert!(message.contains("launchctl"), "{message}");
    }

    /// 2026-07-29T12:00:00Z
    const NOW: i64 = 1_785_326_400;

    fn window(utilization: f64, reset: i64, status: &str) -> Window {
        Window {
            utilization: Some(utilization),
            status: Some(status.to_owned()),
            reset: Some(reset),
            reset_iso: Some(llm_gateway::credential::time::format_rfc3339(reset)),
        }
    }

    fn observed() -> CredentialUsage {
        CredentialUsage::new(
            "claude-personal",
            "claude_oauth",
            llm_gateway::quota::Support::Observed,
            Some(llm_gateway::quota::Snapshot {
                observed_at: NOW - 120,
                observed_at_iso: llm_gateway::credential::time::format_rfc3339(NOW - 120),
                five_hour: Some(window(0.71, NOW + 960, "allowed")),
                seven_day: Some(window(0.3, NOW + 86_400 * 4, "allowed")),
                overage: None,
            }),
        )
    }

    fn report(credentials: Vec<CredentialUsage>) -> Report {
        Report::new(NOW, credentials)
    }

    /// util / 窓の経過率 / 残り時間 を `%/%/時間` の形で出す
    /// (claude-statusline `dualInfo` と同じ組み立て)。
    #[test]
    fn renders_percentages_and_time_left() {
        let out = render(&report(vec![observed()]));

        assert!(out.contains("71%/95%/0h16m"), "5h 窓:\n{out}");
        assert!(out.contains("30%/43%/04d00h"), "7d 窓:\n{out}");
        assert!(
            !out.contains("分前"),
            "取得から間もないので古さは出さない:\n{out}"
        );
    }

    /// モデル別の枠は、表の下に 1 行で出す。
    ///
    /// 5 時間 / 7 日の欄には収まらず、応答ヘッダにも現れないので、ここに
    /// 出さないと利用者から永久に見えない。
    #[test]
    fn a_scoped_limit_gets_its_own_line() {
        let mut c = observed();
        c.limits = Some(vec![
            llm_gateway::quota::QuotaLimit {
                kind: "weekly_all".to_owned(),
                percent: 100.0,
                severity: Some("critical".to_owned()),
                resets_at: Some("2026-08-02T08:59:59.571539+00:00".to_owned()),
                model: None,
                model_id: None,
                is_active: true,
            },
            llm_gateway::quota::QuotaLimit {
                kind: "weekly_scoped".to_owned(),
                percent: 80.0,
                severity: Some("warning".to_owned()),
                resets_at: Some("2026-08-02T08:59:59.571875+00:00".to_owned()),
                model: Some("Fable".to_owned()),
                model_id: None,
                is_active: false,
            },
        ]);

        let out = render(&report(vec![c]));
        assert!(out.contains("Fable の枠は 80%"), "{out}");
        assert!(
            out.contains("2026-08-02T08:59:59+00:00 に開きます"),
            "端数の秒は落とす:\n{out}"
        );
        assert!(
            !out.contains("weekly_all"),
            "モデルの付かない枠は、表の欄と重なるので出さない:\n{out}"
        );
    }

    #[test]
    fn a_timestamp_without_a_fraction_is_left_alone() {
        assert_eq!(
            to_the_second("2026-08-02T08:59:59Z"),
            "2026-08-02T08:59:59Z"
        );
        assert_eq!(
            to_the_second("2026-08-02T08:59:59.5Z"),
            "2026-08-02T08:59:59Z"
        );
        assert_eq!(to_the_second("いつか"), "いつか");
    }

    /// 取得から 5 分を超えたスナップショットだけ古さを添える。
    #[test]
    fn shows_age_only_when_snapshot_is_stale() {
        let mut c = observed();
        if let Some(s) = c.snapshot.as_mut() {
            s.observed_at = NOW - 400;
        }
        let out = render(&report(vec![c]));
        assert!(out.contains("(6 分前)"), "{out}");
    }

    /// 再起動を跨いで読み戻した値には、それだけの経過が付く。
    ///
    /// 永続化した利用状況は取得時刻ごと戻る (DR-0007) ので、古さの表示が
    /// そのまま鮮度の判断材料になる。ここが効かないと、いつの値か分からない
    /// ものを最新として読むことになる。
    #[test]
    fn a_snapshot_restored_from_disk_shows_how_old_it_is() {
        let mut c = observed();
        if let Some(s) = c.snapshot.as_mut() {
            s.observed_at = NOW - 3 * 3600;
        }
        let out = render(&report(vec![c]));
        assert!(out.contains("(3 時間前)"), "{out}");
    }

    /// 未観測・対象外も行として並ぶ (名前ごと消さない)。
    #[test]
    fn renders_a_row_for_credentials_without_numbers() {
        let out = render(&report(vec![
            CredentialUsage::new(
                "bedrock",
                "bedrock_api_key",
                llm_gateway::quota::Support::NotApplicable,
                None,
            ),
            CredentialUsage::new(
                "claude-work",
                "claude_oauth",
                llm_gateway::quota::Support::Unobserved,
                None,
            ),
        ]));

        assert!(out.contains("bedrock"), "{out}");
        assert!(out.contains("not_applicable"), "{out}");
        assert!(out.contains("claude-work"), "{out}");
        assert!(out.contains("unobserved"), "{out}");
    }

    /// 表の下に出すのはプローブの失敗だけ。window の status や overage は
    /// 使用率の % で足りる情報の重複なので出さない (kawaz 裁定。JSON には残る)。
    #[test]
    fn calls_out_probe_failures_only() {
        let mut hit = observed();
        if let Some(s) = hit.snapshot.as_mut() {
            s.five_hour = Some(window(1.0, NOW + 600, "rejected"));
            s.overage = Some(llm_gateway::quota::Overage {
                status: Some("disabled".to_owned()),
                disabled_reason: Some("out_of_credits".to_owned()),
            });
        }
        let mut broken = CredentialUsage::new(
            "nowhere",
            "claude_oauth",
            llm_gateway::quota::Support::Unobserved,
            None,
        );
        broken.probe_error = Some("繋がりません".to_owned());

        let out = render(&report(vec![hit, broken]));
        assert!(!out.contains("rejected です"), "{out}");
        assert!(!out.contains("out_of_credits"), "{out}");
        assert!(out.contains("nowhere: 取り直せませんでした"), "{out}");
    }

    /// バーの ▀ が上下の行と溶けないよう、credential の間は空行で区切る。
    #[test]
    fn rows_are_separated_by_a_blank_line() {
        let out = render(&report(vec![observed(), observed()]));
        assert!(out.contains("\n\n"), "{out}");
    }

    #[test]
    fn probe_cost_is_not_shown() {
        let mut r = report(vec![observed()]);
        r.probe = Some(llm_gateway::quota::Probe {
            requests: 2,
            model: "claude-haiku-4-5-20251001".to_owned(),
            input_tokens: 16,
            output_tokens: 2,
        });

        // 消費報告は出さない (kawaz 裁定)。JSON 側には残る。
        let out = render(&r);
        assert!(!out.contains("claude-haiku-4-5-20251001"), "{out}");
        assert!(!out.contains("トークン消費"), "{out}");
    }

    /// 名前の列幅が揃う。日本語を 1 桁で数えると、名前の長さで崩れる。
    #[test]
    fn names_are_padded_to_the_widest() {
        let out = render(&report(vec![
            observed(),
            CredentialUsage::new(
                "b",
                "relay",
                llm_gateway::quota::Support::UpstreamDependent,
                None,
            ),
        ]));

        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).take(2).collect();
        assert_eq!(lines.len(), 2, "{out}");
        // "claude-personal" が最長なので、2 列目はその幅 + 2 桁の空白から始まる。
        assert!(lines[0].starts_with("claude-personal  "), "{out}");
        assert!(
            lines[1].starts_with(&format!("b{}", " ".repeat(16))),
            "{out}"
        );
    }

    #[test]
    fn width_counts_japanese_as_two_columns() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("名前"), 4);
        assert_eq!(width("あと 16 分"), 4 + 6);
    }

    #[test]
    fn elapsed_time_is_readable() {
        assert_eq!(elapsed(0), "たった今");
        assert_eq!(elapsed(59), "たった今");
        assert_eq!(elapsed(120), "2 分前");
        assert_eq!(elapsed(7200), "2 時間前");
        assert_eq!(elapsed(86_400 * 2), "2 日前");
    }

    #[test]
    fn format_duration_switches_units_by_magnitude() {
        assert_eq!(format_duration(-1, false), "0h00m");
        assert_eq!(format_duration(59, false), "0h00m");
        assert_eq!(format_duration(181, false), "0h03m");
        assert_eq!(format_duration(3660, false), "1h01m");
        assert_eq!(format_duration(36_060, false), "10h01m");
        assert_eq!(format_duration(90_000 + 3600, false), "1d02h");
    }

    /// 7d 窓用の wide は、日〜分のどこでも 6 桁に揃う。
    #[test]
    fn wide_duration_is_always_six_columns() {
        assert_eq!(format_duration(59, true), "00h00m");
        assert_eq!(format_duration(32 * 60, true), "00h32m");
        assert_eq!(format_duration(3660, true), "01h01m");
        assert_eq!(format_duration(36_060, true), "10h01m");
        assert_eq!(format_duration(90_000 + 3600, true), "01d02h");
        assert_eq!(format_duration(86_400 * 10 + 3600, true), "10d01h");
    }

    #[test]
    fn calc_elapsed_reads_back_the_window_progress() {
        // 5h 窓、リセットまで残り 1h ⇒ 窓の 80% が経過している。
        assert_eq!(calc_elapsed(NOW + 3600, 5 * 3600, NOW), 80.0);
        // 経過率は 0〜100 にクランプする。
        assert_eq!(calc_elapsed(NOW - 3600, 5 * 3600, NOW), 100.0);
        assert_eq!(calc_elapsed(NOW + 5 * 3600 + 3600, 5 * 3600, NOW), 0.0);
    }

    #[test]
    fn util_color_thresholds() {
        assert_eq!(util_color(0.0), 40);
        assert_eq!(util_color(49.9), 40);
        assert_eq!(util_color(50.0), 220);
        assert_eq!(util_color(79.9), 220);
        assert_eq!(util_color(80.0), 196);
    }

    /// help に usage の使い方が載っている (実装と説明を揃える)。
    #[test]
    fn usage_documents_the_usage_command() {
        assert!(USAGE.contains("usage"), "{USAGE}");
        assert!(USAGE.contains("--refresh"), "{USAGE}");
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

    // ---------- stats (DR-0011) ----------

    fn counters(reqs: u64, input: u64, output: u64) -> Counters {
        let mut tokens = llm_gateway::metering::TokenUsage::default();
        tokens.set(TokenKind::input(), input);
        tokens.set(TokenKind::output(), output);
        Counters {
            requests: reqs,
            tokens,
        }
    }

    /// 試験で書く 1 行。`(認証情報, モデル, 集計)`。
    type Row<'a> = (&'a str, &'a str, Counters);

    fn stats_report(days: &[(&str, &[Row<'_>])]) -> StatsReport {
        // 単価表を通した後の形を作る。ここで金額まで計算しておくと、
        // 表示側の試験が単価表の中身に引きずられない。
        let mut by_date = serde_json::Map::new();
        let mut grand: Option<f64> = None;
        for (date, rows) in days {
            let mut creds = serde_json::Map::new();
            let mut day_total: Option<f64> = None;
            for (cred, model, c) in *rows {
                let usd = llm_gateway::preset::pricing::for_model(model).map(|p| p.cost(&c.tokens));
                if let Some(usd) = usd {
                    day_total = Some(day_total.unwrap_or(0.0) + usd);
                }
                let mut entry = serde_json::to_value(c).unwrap();
                if let Some(usd) = usd {
                    entry["usd"] = serde_json::json!(usd);
                }
                creds
                    .entry((*cred).to_owned())
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .unwrap()
                    .insert((*model).to_owned(), entry);
            }
            if let Some(total) = day_total {
                grand = Some(grand.unwrap_or(0.0) + total);
            }
            by_date.insert(
                (*date).to_owned(),
                serde_json::json!({"credentials": creds, "total_usd": day_total}),
            );
        }
        // JSON を経由するのは、server が返す形をそのまま読めることも一緒に
        // 確かめられるため。
        serde_json::from_value(serde_json::json!({
            "generated_at": 1_785_326_400_i64,
            "generated_at_iso": "2026-07-29T12:00:00Z",
            "days": by_date,
            "total_usd": grand,
        }))
        .unwrap()
    }

    /// 日ごとの合計と、認証情報 × モデルの内訳が出る。
    #[test]
    fn stats_shows_a_total_and_a_breakdown() {
        let out = render_stats(&stats_report(&[(
            "2026-07-29",
            &[
                ("claude-one", "claude-haiku-4-5", counters(3, 1_200, 340)),
                ("claude-two", "claude-opus-5", counters(1, 50_000, 8_000)),
            ],
        )]));

        assert!(out.contains("2026-07-29"), "{out}");
        // 合計は 4 本 / input 51,200 / output 8,340。
        assert!(out.contains("51,200"), "3 桁区切りの合計: {out}");
        assert!(out.contains("8,340"), "{out}");
        assert!(out.contains("claude-one"), "内訳に認証情報が出る: {out}");
        assert!(out.contains("claude-opus-5"), "内訳にモデルが出る: {out}");
        for head in [REQUESTS_HEADER, "input", "output", USD_HEADER] {
            assert!(out.contains(head), "見出し {head} が無い: {out}");
        }
    }

    /// 記録に現れた区分だけが列になる。使っていない区分で桁を広げない。
    #[test]
    fn stats_columns_follow_the_kinds_that_were_recorded() {
        let out = render_stats(&stats_report(&[(
            "2026-07-29",
            &[("a", "claude-opus-5", counters(1, 10, 5))],
        )]));

        let head = out.lines().next().expect("見出しの行");
        assert!(head.contains("input") && head.contains("output"), "{out}");
        assert!(
            !head.contains("cache_w"),
            "使っていない区分は出さない: {out}"
        );
    }

    /// provider 固有の区分も列になる。潰すと、その消費が表から消える。
    #[test]
    fn a_provider_specific_kind_gets_its_own_column() {
        let mut c = counters(1, 10, 5);
        c.tokens.set(TokenKind::output_reasoning(), 7);
        c.tokens.set("provider.batch_prediction", 3);

        let out = render_stats(&stats_report(&[("2026-07-29", &[("a", "m", c)])]));

        let head = out.lines().next().expect("見出しの行");
        assert!(head.contains("reasoning"), "標準区分は短い見出し: {out}");
        assert!(
            head.contains("provider.batch_prediction"),
            "知らない区分は名前のまま出す: {out}"
        );
        let row = out
            .lines()
            .find(|l| l.starts_with("  a "))
            .expect("内訳の行");
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>(),
            // 認証情報 / モデル / 本数 / input / output / reasoning / 固有区分 / USD
            ["a", "m", "1", "10", "5", "7", "3", "-"],
            "{out}"
        );
    }

    /// 単価表に無いモデルの金額欄は `-`。0.0000 と出すと「使ったが安かった」に
    /// 見えるので、言えることが無いことを記号で示す。
    #[test]
    fn an_unpriced_model_shows_a_dash() {
        let out = render_stats(&stats_report(&[(
            "2026-07-29",
            &[
                ("a", "claude-opus-5", counters(1, 1_000_000, 0)),
                ("a", "who-knows", counters(1, 1_000_000, 0)),
            ],
        )]));

        let unpriced = out
            .lines()
            .find(|l| l.contains("who-knows"))
            .expect("内訳の行");
        assert!(unpriced.trim_end().ends_with('-'), "{out}");
        // 合計は値付けできた分だけ ($5)。
        assert!(out.contains("5.0000"), "{out}");
    }

    /// 新しい日が上に来る。見たいのは直近。
    #[test]
    fn stats_puts_the_newest_day_first() {
        let out = render_stats(&stats_report(&[
            ("2026-07-28", &[("a", "m", counters(1, 1, 1))]),
            ("2026-07-30", &[("a", "m", counters(1, 2, 2))]),
        ]));

        let newest = out.find("2026-07-30").expect("新しい日");
        let oldest = out.find("2026-07-28").expect("古い日");
        assert!(newest < oldest, "新しい日が先: {out}");
    }

    /// 桁は全日で揃える。日ごとに幅が変わると縦に読めない。
    #[test]
    fn stats_aligns_columns_across_days() {
        let out = render_stats(&stats_report(&[
            ("2026-07-29", &[("a", "m", counters(1, 1_000_000, 1))]),
            ("2026-07-30", &[("a", "m", counters(1, 5, 1))]),
        ]));

        // 内訳の行だけ集めて、数の始まる桁が揃っているか見る。
        let starts: Vec<usize> = out
            .lines()
            .filter(|l| l.starts_with("  a "))
            .map(|l| l.find(|c: char| c.is_ascii_digit() || c == ',').unwrap())
            .collect();
        assert_eq!(starts.len(), 2, "{out}");
        assert_eq!(starts[0], starts[1], "桁が揃う: {out}");
    }

    /// 記録が無ければ、何をすれば出るのかを言う。
    ///
    /// 文言は英語 (DR-0008: CLI が出す文言は英語)。
    #[test]
    fn empty_stats_say_why() {
        let out = render_stats(&stats_report(&[]));
        assert!(out.contains("No usage recorded yet"), "{out}");
        assert!(out.contains("gateway"), "次に見る所を示す: {out}");
        assert!(out.contains("stats directory"), "置き場も示す: {out}");
    }

    /// 認証情報を持たない経路は予約名で出る。
    #[test]
    fn stats_show_the_credentialless_route() {
        let out = render_stats(&stats_report(&[(
            "2026-07-29",
            &[(llm_gateway::stats::NO_CREDENTIAL, "m", counters(1, 5, 6))],
        )]));
        assert!(out.contains(llm_gateway::stats::NO_CREDENTIAL), "{out}");
    }

    #[test]
    fn thousands_separates_every_three_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(12_345), "12,345");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    fn parse_stats(list: &[&str]) -> Result<StatsArgs, String> {
        parse_stats_args(&args(list))
    }

    #[test]
    fn stats_defaults_to_a_week() {
        assert_eq!(parse_stats(&[]).unwrap().days, 7);
    }

    #[test]
    fn stats_takes_a_day_count() {
        assert_eq!(parse_stats(&["--days", "30"]).unwrap().days, 30);
        assert_eq!(parse_stats(&["--days=1"]).unwrap().days, 1);
        assert_eq!(parse_stats(&["--days", "0"]).unwrap().days, 0, "0 は全期間");
    }

    #[test]
    fn stats_takes_a_config_path() {
        let parsed = parse_stats(&["--config", "/tmp/c.toml"]).unwrap();
        assert_eq!(parsed.config_path, PathBuf::from("/tmp/c.toml"));
        assert_eq!(parsed.days, 7);
        assert_eq!(
            parse_stats(&["--config=/tmp/d.toml", "--days=3"])
                .unwrap()
                .config_path,
            PathBuf::from("/tmp/d.toml")
        );
    }

    /// 読めない日数は断る。黙って既定に落とすと、絞ったつもりの相手が別の
    /// 範囲を見ることになる。
    #[test]
    fn stats_rejects_an_unreadable_day_count() {
        for bad in [vec!["--days", "lots"], vec!["--days=-1"], vec!["--days"]] {
            assert!(parse_stats(&bad).is_err(), "{bad:?}");
        }
    }

    /// 大きすぎる日数は上限で抑える (桁あふれで何も返らなくなるのを防ぐ)。
    #[test]
    fn stats_clamps_an_enormous_day_count() {
        let max = llm_gateway::stats::MAX_DAYS;
        assert_eq!(parse_stats(&["--days", "100000"]).unwrap().days, max);
        assert_eq!(
            parse_stats(&[&format!("--days={}", usize::MAX)])
                .unwrap()
                .days,
            max
        );
    }

    #[test]
    fn stats_rejects_unknown_options() {
        assert!(parse_stats(&["--refresh"]).is_err());
    }

    /// 問い合わせ先は待ち受け先から作る。受け口の指定は loopback に読み替える。
    #[test]
    fn the_stats_url_points_at_the_local_server() {
        assert_eq!(
            gateway_url("127.0.0.1:8402", "stats"),
            "http://127.0.0.1:8402/llm-gateway/stats"
        );
        assert_eq!(
            gateway_url("0.0.0.0:8402", "stats"),
            "http://127.0.0.1:8402/llm-gateway/stats"
        );
        assert_eq!(
            gateway_url("[::]:8402", "stats"),
            "http://127.0.0.1:8402/llm-gateway/stats"
        );
    }

    /// usage の宛先は変わっていない。
    #[test]
    fn the_usage_url_is_unchanged() {
        assert_eq!(
            usage_url("127.0.0.1:11300", false),
            "http://127.0.0.1:11300/llm-gateway/usage"
        );
        assert_eq!(
            usage_url("127.0.0.1:11300", true),
            "http://127.0.0.1:11300/llm-gateway/usage?refresh=true"
        );
    }

    /// help に stats が並ぶ。
    #[test]
    fn help_mentions_stats() {
        assert!(USAGE.contains("stats"), "コマンド一覧に無い");
        assert!(USAGE.contains("--days"), "オプションの説明が無い");
    }

    /// 表の形を丸ごと固定する。
    ///
    /// 桁揃えは目で見て決めたもので、崩れても個別の assert には出にくい。
    /// 1 度だけの見出し・日ごとの合計・字下げした内訳・日の間の空行を、
    /// まとめてここで見張る。1 行にしか出てこない区分 (ここでは cache) も
    /// 列として立ち、他の行はその欄が 0 になる。
    #[test]
    fn the_table_keeps_its_shape() {
        let mut cached = counters(12, 98_120, 7_431);
        cached.tokens.set(TokenKind::input_cache_creation(), 2_000);
        cached.tokens.set(TokenKind::input_cache_read(), 50_000);

        let out = render_stats(&stats_report(&[
            (
                "2026-07-29",
                &[
                    (
                        "claude-one",
                        "claude-haiku-4-5",
                        counters(312, 1_204_887, 34_002),
                    ),
                    ("claude-two", "claude-opus-5", cached),
                ],
            ),
            (
                "2026-07-30",
                &[("claude-one", "claude-haiku-4-5", counters(7, 8_120, 931))],
            ),
        ]));

        let expected = concat!(
            "                                   reqs     input    output   cache_w   cache_r    usd\n",
            "2026-07-30 total                      7     8,120       931         0         0 0.0128\n",
            "  claude-one claude-haiku-4-5         7     8,120       931         0         0 0.0128\n",
            "\n",
            "2026-07-29 total                    324 1,303,007    41,433     2,000    50,000 2.0888\n",
            "  claude-one claude-haiku-4-5       312 1,204,887    34,002         0         0 1.3749\n",
            "  claude-two claude-opus-5           12    98,120     7,431     2,000    50,000 0.7139\n",
            "\n",
            "total                               331 1,311,127    42,364     2,000    50,000 2.1015\n",
        );
        assert_eq!(
            out, expected,
            "\n--- 実際 ---\n{out}--- 期待 ---\n{expected}"
        );
    }
}
