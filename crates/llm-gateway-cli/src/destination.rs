//! 稼働中の 1 台へ問い合わせる先を、登録簿から引く (DR-0028 決定 6)。
//!
//! `--config` で設定ファイルを渡させると、聞くだけの命令のために待ち受けない
//! 設定を置くことになる。宛先を覚えているのは登録簿の役目で、CLI は名前で指す。

use llm_gateway::daemon::registry::Registry;

use crate::failure::Failure;

/// 聞きに行く先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// どの台に聞いているか (出力に添えると、複数台のときに読み手が迷わない)。
    pub unit: String,
    /// その台の設定に書かれた `[server] listen`。
    pub listen: String,
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
    Ok(Target {
        unit: name,
        listen: config.server.listen,
    })
}

/// `/llm-gateway/<name>` の宛先。
///
/// 設定の `listen` は待ち受け側の書き方なので、どこからでも受ける指定
/// (`0.0.0.0` / `::`) をそのまま宛先にはできない。手元から叩く前提で
/// loopback に読み替える。
pub fn gateway_url(listen: &str, name: &str) -> String {
    let host = match listen.rsplit_once(':') {
        Some((host, port)) => {
            let host = match host.trim_matches(['[', ']']) {
                "" | "0.0.0.0" | "::" => "127.0.0.1",
                other => other,
            };
            if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            }
        }
        None => listen.to_owned(),
    };
    format!("http://{host}/llm-gateway/{name}")
}

/// 選んだ台に聞いて、返ってきた報告を読む。
///
/// 報告を持っているのは走っている側なので、CLI は聞いて整形するだけ
/// (DR-0007)。CLI 単独では何も答えられない。
pub fn ask<T: serde::de::DeserializeOwned>(
    target: &Target,
    name: &str,
    query: &str,
) -> Result<T, Failure> {
    let url = format!("{}{query}", gateway_url(&target.listen, name));
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))?;

    runtime.block_on(async move {
        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .map_err(|e| unreachable(target, &e.to_string()))?;
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

/// 届かなかったときに、次に何を見ればよいかまで言う。
pub fn unreachable(target: &Target, reason: &str) -> Failure {
    Failure::new(
        "unit_unreachable",
        format!(
            "could not reach `{}` at {} ({reason}). \
check that it is running (`llm-gateway daemon status`)",
            target.unit, target.listen
        ),
    )
    .with("unit", target.unit.clone())
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

        assert_eq!(
            resolve(&registry, None).unwrap(),
            Target {
                unit: "unstable".to_owned(),
                listen: "127.0.0.1:11301".to_owned()
            }
        );
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

        assert_eq!(
            resolve(&registry, Some("stable")).unwrap().listen,
            "127.0.0.1:11302"
        );
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

    /// 待ち受けの書き方をそのまま宛先にしない。
    ///
    /// `0.0.0.0` は「どこからでも受ける」の意味で、繋ぎに行く先ではない。
    #[test]
    fn listen_address_becomes_a_reachable_url() {
        assert_eq!(
            gateway_url("127.0.0.1:11300", "usage"),
            "http://127.0.0.1:11300/llm-gateway/usage"
        );
        assert_eq!(
            gateway_url("0.0.0.0:11300", "usage"),
            "http://127.0.0.1:11300/llm-gateway/usage"
        );
        assert_eq!(
            gateway_url("[::]:11300", "stats"),
            "http://127.0.0.1:11300/llm-gateway/stats"
        );
        assert_eq!(
            gateway_url("[::1]:11300", "usage"),
            "http://[::1]:11300/llm-gateway/usage"
        );
    }

    /// 届かないときは、どの台のどこへ行ったのかを残す。
    #[test]
    fn an_unreachable_unit_says_which_one_and_where() {
        let target = Target {
            unit: "unstable".to_owned(),
            listen: "127.0.0.1:11301".to_owned(),
        };
        let e = unreachable(&target, "connection refused");
        assert_eq!(e.kind(), "unit_unreachable");
        assert!(e.message().contains("unstable"), "{e:?}");
        assert!(e.message().contains("127.0.0.1:11301"), "{e:?}");
        assert!(e.message().contains("daemon status"), "{e:?}");
    }
}
