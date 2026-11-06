//! 稼働中の 1 台へ問い合わせる先を、登録簿から引く (DR-0028 決定 6)。
//!
//! `--config` で設定ファイルを渡させると、聞くだけの命令のために待ち受けない
//! 設定を置くことになる。宛先を覚えているのは登録簿の役目で、CLI は名前で指す。

use std::path::PathBuf;

use llm_gateway::daemon::registry::Registry;

use crate::failure::Failure;

/// 聞きに行く先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// どの台に聞いているか (出力に添えると、複数台のときに読み手が迷わない)。
    pub unit: String,
    /// その台の運用系の口 (`[server] unix`、DR-0043 決定 7)。
    pub socket: PathBuf,
}

/// 名前で 1 台を選ぶ。省略されたら、1 台しか無いときに限ってそれを使う。
///
/// 複数あって未指定のときに既定を作らない。どちらに聞いたか分からないまま
/// 数字を読むと、面を取り違えたことに気づけない。
pub fn resolve(registry: &Registry, unit: Option<&str>) -> Result<Target, Failure> {
    let name = match unit {
        Some(name) => name.to_owned(),
        None => {
            let names = registry.list()?;
            match names.len() {
                0 => {
                    return Err(Failure::new(
                        "no_units",
                        "no unit is registered. \
`llm-gateway daemon add <config>` registers one",
                    ));
                }
                1 => names[0].0.clone(),
                _ => {
                    return Err(Failure::new(
                        "unit_required",
                        "more than one unit is registered. say which one with --unit <name>",
                    )
                    .with(
                        "units",
                        names.into_iter().map(|(name, _)| name).collect::<Vec<_>>(),
                    ));
                }
            }
        }
    };

    let registered = registry.get(&name)?;
    let config = llm_gateway::Config::load(&registered.config).map_err(|e| {
        Failure::new(
            "unit_config_unreadable",
            format!(
                "could not read the configuration of `{name}` ({}): {e}",
                registered.config.display()
            ),
        )
    })?;
    let socket = config.server.unix_path(&name);
    Ok(Target { unit: name, socket })
}

/// 選んだ台に聞いて、返ってきた報告を読む。
///
/// 報告を持っているのは走っている側なので、CLI は聞いて整形するだけ
/// (DR-0007)。CLI 単独では何も答えられない。
///
/// 読むのは台の Unix socket の口で、TCP (`[server] listen`) には行かない。TCP の口は
/// `[callers]` のある台では署名を求め、CLI は呼び手の鍵を持たない (DR-0043 決定 7)。
/// socket に繋げない時も TCP に落ちず、繋げない理由を言って断る。
pub fn ask<T: serde::de::DeserializeOwned>(
    target: &Target,
    name: &str,
    query: &str,
) -> Result<T, Failure> {
    // socket の上の HTTP なので host は飾り。台は host を見ない。
    let url = format!("http://localhost/llm-gateway/{name}{query}");
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))?;

    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .unix_socket(target.socket.as_path())
            .build()
            .map_err(|e| Failure::from(format!("could not prepare the HTTP client: {e}")))?;
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| unreachable(target, &causes(&e)))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Failure::from(format!("could not read the response: {e}")))?;
        if !status.is_success() {
            return Err(Failure::new(
                "unit_refused",
                format!("`{}` returned {status}: {body}", target.unit),
            ));
        }
        serde_json::from_str(&body)
            .map_err(|e| Failure::from(format!("could not parse the response: {e}")))
    })
}

/// エラーとその原因を順に並べる。reqwest の表の文は「送れなかった」だけで、socket が
/// 無いのか誰も待っていないのかは原因の側にある。
fn causes(e: &dyn std::error::Error) -> String {
    let mut said = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        said.push_str(": ");
        said.push_str(&cause.to_string());
        source = cause.source();
    }
    said
}

/// 届かなかったときに、次に何を見ればよいかまで言う。
///
/// socket を開かない古い版の台が走っている場合もここに来る (TCP には落ちないので)。
pub fn unreachable(target: &Target, reason: &str) -> Failure {
    Failure::new(
        "unit_unreachable",
        format!(
            "could not reach `{unit}` through its operations socket {socket} ({reason}). \
check that it is running (`llm-gateway daemon status`); a unit running a version that \
does not open this socket needs updating and `llm-gateway daemon restart {unit}`",
            unit = target.unit,
            socket = target.socket.display()
        ),
    )
    .with("unit", target.unit.clone())
    .with("socket", target.socket.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_gateway::daemon::registry::Unit;
    use std::path::Path;

    fn register(registry: &Registry, dir: &Path, name: &str, listen: &str) {
        let config = dir.join(format!("{name}.toml"));
        std::fs::write(&config, format!("[server]\nlisten = \"{listen}\"\n")).unwrap();
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

    /// 1 台しか無ければ、名前を打たなくても宛先が決まる。
    #[test]
    fn the_only_unit_is_the_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        register(&registry, dir.path(), "unstable", "127.0.0.1:11301");

        assert_eq!(resolve(&registry, None).unwrap().unit, "unstable");
    }

    /// 複数あるのに指されなかったら、勝手に選ばずに名前を求める。
    #[test]
    fn more_than_one_unit_needs_a_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        register(&registry, dir.path(), "stable", "127.0.0.1:11302");
        register(&registry, dir.path(), "unstable", "127.0.0.1:11301");

        let e = resolve(&registry, None).unwrap_err();
        assert_eq!(e.kind(), "unit_required");
        let json: serde_json::Value = serde_json::from_str(&e.to_json()).unwrap();
        assert_eq!(
            json["error"]["units"],
            serde_json::json!(["stable", "unstable"])
        );

        assert_eq!(resolve(&registry, Some("stable")).unwrap().unit, "stable");
    }

    /// 1 台も無いときは、登録の仕方を言う。
    #[test]
    fn no_unit_at_all_says_how_to_register_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));

        let e = resolve(&registry, None).unwrap_err();
        assert_eq!(e.kind(), "no_units");
        assert!(e.message().contains("daemon add"), "{e:?}");
    }

    #[test]
    fn an_unknown_name_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        register(&registry, dir.path(), "stable", "127.0.0.1:11302");

        assert_eq!(
            resolve(&registry, Some("nope")).unwrap_err().kind(),
            "unknown_unit"
        );
    }

    /// 登録はあるのに設定が読めない状態を、「台が無い」と言わない。
    #[test]
    fn a_registered_unit_with_an_unreadable_configuration_says_so() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        register(&registry, dir.path(), "stable", "127.0.0.1:11302");
        std::fs::remove_file(dir.path().join("stable.toml")).unwrap();

        let e = resolve(&registry, Some("stable")).unwrap_err();
        assert_eq!(e.kind(), "unit_config_unreadable");
        assert!(e.message().contains("stable"), "{e:?}");
    }

    /// 聞きに行く socket は台と同じ規則で引く: 書かなければ unit ごとの既定の置き場、
    /// 書けばその path。
    #[test]
    fn the_socket_is_the_one_the_unit_opens() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        register(&registry, dir.path(), "stable", "127.0.0.1:11302");
        let written = dir.path().join("written.toml");
        std::fs::write(&written, "[server]\nunix = \"/run/gw/u.sock\"\n").unwrap();
        registry
            .add(
                "written",
                &Unit {
                    config: written,
                    binary_path: PathBuf::from("/usr/local/bin/llm-gateway"),
                    enabled: true,
                    added_at: "2026-09-09T00:00:00Z".to_owned(),
                },
            )
            .unwrap();

        let stable = resolve(&registry, Some("stable")).unwrap();
        assert_eq!(
            stable.socket,
            llm_gateway::config::Server::default().unix_path("stable")
        );
        assert!(
            stable.socket.ends_with("daemon/http/stable.sock"),
            "{}",
            stable.socket.display()
        );
        assert_eq!(
            resolve(&registry, Some("written")).unwrap().socket,
            PathBuf::from("/run/gw/u.sock")
        );
    }

    /// 台を socket の口で立てる (別スレッドの runtime。`ask` は自分の runtime を回す)。
    /// 返すのは立てた socket の path。
    fn serve_fixture(dir: &Path, router: axum::Router) -> PathBuf {
        let path = dir.join("u.sock");
        let (ready, bound) = std::sync::mpsc::channel();
        let at = path.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let socket = llm_gateway_server::socket::OperationsSocket::bind(&at).unwrap();
                ready.send(()).unwrap();
                let _ = axum::serve(socket, router.into_make_service()).await;
            });
        });
        bound.recv().unwrap();
        path
    }

    /// CLI は台の socket の口を読む (TCP には行かない)。
    #[test]
    fn ask_reads_the_unit_through_its_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = axum::Router::new().route(
            "/llm-gateway/usage",
            axum::routing::get(|query: axum::extract::RawQuery| async move {
                axum::Json(serde_json::json!({"query": query.0}))
            }),
        );
        let target = Target {
            unit: "unstable".to_owned(),
            socket: serve_fixture(dir.path(), router),
        };

        let answer: serde_json::Value = ask(&target, "usage", "?refresh=true").unwrap();
        assert_eq!(answer, serde_json::json!({"query": "refresh=true"}));

        let e = ask::<serde_json::Value>(&target, "stats", "").unwrap_err();
        assert_eq!(e.kind(), "unit_refused", "{e:?}");
        assert!(e.message().contains("404"), "{e:?}");
    }

    /// socket が無ければ (止まっている、socket を開かない古い版が走っている) TCP に
    /// 落ちずに断り、どの socket を見たかと、版を上げて restart することを言う。
    #[test]
    fn a_missing_socket_is_refused_with_how_to_fix_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = Target {
            unit: "unstable".to_owned(),
            socket: dir.path().join("absent.sock"),
        };

        let e = ask::<serde_json::Value>(&target, "usage", "").unwrap_err();
        assert_eq!(e.kind(), "unit_unreachable");
        let said = e.message();
        assert!(said.contains("absent.sock"), "{e:?}");
        assert!(said.contains("daemon status"), "{e:?}");
        assert!(said.contains("daemon restart unstable"), "{e:?}");
        let json: serde_json::Value = serde_json::from_str(&e.to_json()).unwrap();
        assert_eq!(json["error"]["unit"], "unstable");
    }

    /// 届かないときは、どの台のどこへ行ったのかを残す。
    #[test]
    fn an_unreachable_unit_says_which_one_and_where() {
        let target = Target {
            unit: "unstable".to_owned(),
            socket: PathBuf::from("/run/gw/unstable.sock"),
        };
        let e = unreachable(&target, "connection refused");
        assert_eq!(e.kind(), "unit_unreachable");
        assert!(e.message().contains("unstable"), "{e:?}");
        assert!(e.message().contains("/run/gw/unstable.sock"), "{e:?}");
        assert!(e.message().contains("daemon status"), "{e:?}");
    }
}
