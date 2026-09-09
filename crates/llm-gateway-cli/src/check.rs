//! 設定を読んで確かめるだけ。起動前の確認に使う。
//!
//! ファイルそのものを対象にする命令なので、`--config` はここに残る
//! (DR-0028 決定 6)。

use std::path::Path;
use std::process::ExitCode;

use llm_gateway::Config;
use llm_gateway::credential::file::FileStore;
use llm_gateway::credential::{CredentialId, Persistence};

use crate::failure::Failure;
use crate::load;

pub fn run(config_path: &Path) -> Result<ExitCode, Failure> {
    let config = load(config_path)?;
    let dir = config.store.resolve_dir();

    println!("config       {}", config_path.display());
    println!("listen       {}", listen_line(&config.server));
    if let Some(binary) = &config.server.binary_path {
        println!("binary       {}", binary.display());
    }
    println!("store        {}", dir.display());
    println!("credentials  {}", config.credentials.len());
    println!("namespaces   {}", config.namespace_names().join(", "));
    for line in namespace_summary_lines(&config) {
        println!("{line}");
    }
    let (destinations, _) = config.webhook.destinations();
    println!(
        "webhook      {}",
        match destinations.len() {
            0 => "not configured".to_owned(),
            1 => destinations[0].to_string(),
            count => format!("{count} endpoints"),
        }
    );

    // 認証情報が置かれているかは、起動しないと分からない部分。ここで見ておくと
    // 動かしてから 401 で気づく事態を減らせる。
    let store = FileStore::open(&dir).map_err(|e| Failure::from(e.to_string()))?;
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

    // 走らせる実行ファイルは登録簿へ焼き込まれる (DR-0028 決定 2)。無い道を
    // 焼き込むと、監督者が上げようとした時点で初めて分かる。
    if let Some(binary) = &config.server.binary_path
        && !binary.exists()
    {
        println!("\nwarning: the binary written in [server] binary_path is not there:");
        println!("  {}", binary.display());
    }

    let unrouted = config.namespaces_without_routing();
    if !unrouted.is_empty() {
        println!("\nwarning: these namespaces have no routing rule, so every model falls back");
        println!("to the declared order of credentials:");
        for name in &unrouted {
            println!("  {name}");
        }
        println!("  write `[[ns.<name>.routing]]` to say which route each model takes");
    }

    let unaliased = config.namespaces_without_aliases();
    if !unaliased.is_empty() {
        println!("\nwarning: these namespaces write no short name of their own:");
        for name in &unaliased {
            println!("  {name}");
        }
        println!("  write `[ns.<name>.aliases]` to add short names of your own");
    }

    let orphaned = config.keepalive_without_destination();
    if !orphaned.is_empty() {
        println!(
            "\nwarning: these namespaces ask for cache keepalive without a webhook destination:"
        );
        for name in &orphaned {
            println!("  {name}");
        }
        println!(
            "  set `webhook.base_url` (or `base_urls`) so the signal can reach the conversation"
        );
    }

    let unpointed = config.status_sources_without_routes();
    if !unpointed.is_empty() {
        println!("\nwarning: no route names these status sources, so they describe nothing:");
        for name in &unpointed {
            println!("  {name}");
        }
        println!("  write `status_source = \"<name>\"` on the routes each source speaks for");
    }

    let unpriced = config.keepalive_horizon_without_pricing(&|model| {
        llm_gateway::preset::pricing::for_model(model).is_some()
    });
    if !unpriced.is_empty() {
        println!("\nwarning: no price is known for these models, so a keepalive horizon");
        println!("written as a share of the break-even time falls back to the default:");
        for (ns_name, model) in &unpriced {
            println!("  {ns_name}: {model}");
        }
    }

    // 単価表は手で書くので、新しいモデルが出ると置いていかれる。動かす前に
    // 見えるのは設定に名前を書いた分だけ (upstream に聞いた一覧は起動しないと
    // 無い) なので、ここで挙がらなくても discovery 側がもう一度見る。
    let gaps = llm_gateway::preset::pricing::gaps(config.declared_model_names());
    if !gaps.is_empty() {
        println!("\nwarning: the price table does not describe these models:");
        for gap in &gaps {
            println!("  {gap}");
        }
        println!("  add a row to `preset/pricing.rs` so the cost is not guessed");
    }

    if missing.is_empty() && unreadable.is_empty() {
        println!("\nno problems found");
        return Ok(ExitCode::SUCCESS);
    }

    if !missing.is_empty() {
        println!("\nthese credentials are not in {}:", dir.display());
        for name in &missing {
            println!("  {name}.json");
        }
        println!("  `llm-gateway login --type <type> <name>` obtains them");
    }
    if !unreadable.is_empty() {
        println!("\nthese credentials could not be read:");
        for (name, reason) in &unreadable {
            println!("  {name}: {reason}");
        }
    }
    Ok(ExitCode::FAILURE)
}

/// namespace ごとの要約行。
///
/// 名前を並べるだけでは、節が丸ごと消えていても「居る」ようにしか見えない。
/// 数を並べると、0 が並んだ行がその場で目に入る。
fn namespace_summary_lines(config: &Config) -> Vec<String> {
    config
        .namespaces
        .iter()
        .map(|(name, ns)| {
            format!(
                "  {name:<11}{} routing, {} aliases, {} cache",
                ns.routing.len(),
                ns.aliases.len(),
                ns.cache.len()
            )
        })
        .collect()
}

/// 待ち受け行。
///
/// 待ち受けない設定でも住所は出す。問い合わせ先はこの値で組むので、伏せると
/// 「どこへ聞きに行くのか」が見えなくなる。
fn listen_line(server: &llm_gateway::config::Server) -> String {
    if server.disabled {
        return format!(
            "{} (disabled: this configuration does not listen)",
            server.listen
        );
    }
    server.listen.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(run(&path).unwrap(), ExitCode::SUCCESS);
    }

    /// 実行ファイルの指定も読める (登録簿へ焼き込まれる値なので、ここで見える)。
    #[test]
    fn a_binary_path_is_read_and_shown() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("dummy.toml");
        std::fs::write(
            &path,
            format!(
                "[server]\nlisten = \"127.0.0.1:11300\"\nbinary_path = \"/opt/homebrew/bin/llm-gateway\"\n\n\
                 [store]\ntype = \"file\"\ndir = \"{}\"\n",
                dir.path().join("creds").display()
            ),
        )
        .unwrap();

        let config = load(&path).unwrap();
        assert_eq!(
            config.server.binary_path,
            Some(std::path::PathBuf::from("/opt/homebrew/bin/llm-gateway"))
        );
        assert_eq!(run(&path).unwrap(), ExitCode::SUCCESS);
    }

    /// 要約行は namespace ごとに 1 行、数がそのまま並ぶ。
    #[test]
    fn the_summary_line_counts_what_each_namespace_wrote() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("dummy.toml");
        std::fs::write(
            &path,
            r#"
[credentials.a]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"

[ns.personal]

[ns.work]
aliases = { fast = "claude-haiku-*" }

[[ns.work.routing]]
models = ["*"]
routes = ["a"]

[[ns.work.cache]]
models = ["*"]
main = "keepalive"
"#,
        )
        .unwrap();
        let config = load(&path).unwrap();

        assert_eq!(
            namespace_summary_lines(&config),
            vec![
                "  personal   0 routing, 0 aliases, 0 cache".to_owned(),
                "  work       1 routing, 1 aliases, 1 cache".to_owned(),
            ]
        );
    }

    /// 待ち受け行は、無効かどうかで書き分ける。住所そのものは伏せない。
    #[test]
    fn the_listen_line_says_when_it_does_not_listen() {
        use llm_gateway::config::Server;

        let listening = Server {
            listen: "127.0.0.1:11300".to_owned(),
            ..Server::default()
        };
        assert_eq!(listen_line(&listening), "127.0.0.1:11300");

        let quiet = Server {
            disabled: true,
            ..listening
        };
        let line = listen_line(&quiet);
        assert!(
            line.contains("127.0.0.1:11300"),
            "the endpoint is visible: {line}"
        );
        assert!(line.contains("disabled"), "{line}");
    }
}
