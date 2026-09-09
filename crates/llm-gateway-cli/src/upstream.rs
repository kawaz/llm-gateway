//! upstream (Anthropic / OpenAI ...) が何と言っているか (DR-0021)。
//!
//! プロセスの生死は `daemon status`、OS への登録は `service status`。語が
//! 3 者で分かれるよう、こちらは `upstream status` に置いてある (DR-0028 決定 5)。

use std::process::ExitCode;

use llm_gateway::daemon::registry::Registry;
use llm_gateway::status::{OfficialState, Report};

use crate::destination;
use crate::failure::Failure;
use crate::help;
use crate::options::{split, take_value};
use crate::text::elapsed_ms;

/// `upstream status` に渡された内容。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    refresh: bool,
    unit: Option<String>,
}

pub fn run(args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() || help::wanted(args) {
        print!("{}", help::UPSTREAM);
        return Ok(ExitCode::SUCCESS);
    }
    match args[0].as_str() {
        "status" => status(&args[1..]),
        other => Err(Failure::from(format!(
            "there is no `upstream {other}` command. see `llm-gateway upstream --help`"
        ))),
    }
}

fn status(args: &[String]) -> Result<ExitCode, Failure> {
    let parsed = parse(args)?;
    let target = destination::resolve(&Registry::open(), parsed.unit.as_deref())?;
    let report: Report = destination::ask(
        &target,
        "status",
        if parsed.refresh { "?refresh=true" } else { "" },
    )?;
    print!("{}", render(&report));
    Ok(ExitCode::SUCCESS)
}

fn parse(args: &[String]) -> Result<Args, Failure> {
    let mut parsed = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("refresh", None)) => parsed.refresh = true,
            Some(("unit", inline)) => parsed.unit = Some(take_value("unit", inline, &mut it)?),
            _ => return Err(Failure::from(format!("could not understand `{arg}`"))),
        }
    }
    Ok(parsed)
}

fn official_state_label(state: OfficialState) -> &'static str {
    match state {
        OfficialState::Operational => "operational",
        OfficialState::Degraded => "degraded",
        OfficialState::PartialOutage => "partial outage",
        OfficialState::MajorOutage => "major outage",
        OfficialState::Maintenance => "maintenance",
        OfficialState::Unknown => "unknown",
    }
}

fn render(report: &Report) -> String {
    let mut out = String::from("SERVICE     STATUS    OFFICIAL         OBSERVED   UPDATED\n");
    for service in &report.services {
        let official = official_state_label(service.official.state);
        let observed = format!("{:?}", service.observed.state).to_lowercase();
        let updated = service
            .official
            .observed_at
            .or(service.observed.observed_at)
            .map(|at| elapsed_ms(report.generated_at.saturating_sub(at)))
            .unwrap_or_else(|| "-".to_owned());
        let stale = if service.official.stale { " stale" } else { "" };
        out.push_str(&format!(
            "{:<11} {:<9} {:<16} {:<10} {}{}\n",
            service.name,
            format!("{:?}", service.severity).to_uppercase(),
            official,
            observed,
            updated,
            stale
        ));
    }
    for service in &report.services {
        for incident in &service.official.incidents {
            out.push_str(&format!(
                "\n{}: {}\n  {}\n",
                service.name, incident.name, incident.url
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    const SECOND: i64 = 1_000;

    #[test]
    fn status_takes_refresh_and_a_unit() {
        assert_eq!(parse(&args(&[])).unwrap(), Args::default());
        let parsed = parse(&args(&["--refresh", "--unit", "stable"])).unwrap();
        assert!(parsed.refresh);
        assert_eq!(parsed.unit.as_deref(), Some("stable"));
        assert!(parse(&args(&["--config=/tmp/c.toml"])).is_err());
    }

    /// 子の名前を間違えたら、その階層の help へ案内する。
    #[test]
    fn an_unknown_subcommand_points_at_the_level_help() {
        let e = run(&args(&["statuss"])).unwrap_err();
        assert!(e.message().contains("upstream --help"), "{e:?}");
    }

    fn status_report(state: OfficialState, stale: bool, incident: bool) -> Report {
        use llm_gateway::status::*;
        Report {
            schema_version: 2,
            generated_at: 100_000,
            overall: Overall {
                severity: Severity::Unknown,
                service_counts: Counts::default(),
            },
            services: vec![Service {
                id: "provider".into(),
                name: "Provider".into(),
                routes: vec!["route".into()],
                severity: if state == OfficialState::Operational {
                    Severity::Ok
                } else {
                    Severity::Warning
                },
                official: Official {
                    state,
                    source: "statuspage_v2".into(),
                    source_url: "https://status.example/".into(),
                    observed_at: Some(90_000),
                    stale,
                    components: vec![],
                    incidents: incident
                        .then(|| Incident {
                            id: "i".into(),
                            name: "Incident".into(),
                            state: "investigating".into(),
                            impact: "minor".into(),
                            created_at: None,
                            updated_at: None,
                            url: "https://stspg.io/i".into(),
                            latest_update: "".into(),
                            scope: None,
                        })
                        .into_iter()
                        .collect(),
                    error: None,
                },
                observed: Observed {
                    state: ObservedState::Unknown,
                    observed_at: None,
                    expires_at: None,
                    last_success_at: None,
                    last_failure: None,
                },
            }],
        }
    }

    /// operational は OK、実測なしは unknown として同じ行に表示する。
    #[test]
    fn status_formatter_shows_operational_and_unknown() {
        let out = render(&status_report(OfficialState::Operational, false, false));
        assert!(
            out.contains("Provider    OK        operational      unknown"),
            "{out}"
        );
    }

    /// 複数語の公式状態は enum の Debug 表現で連結せず、利用者向けに空白で区切る。
    #[test]
    fn official_state_formatter_separates_multiword_states() {
        assert_eq!(
            official_state_label(OfficialState::PartialOutage),
            "partial outage"
        );
        assert_eq!(
            official_state_label(OfficialState::MajorOutage),
            "major outage"
        );
    }

    /// incident は一覧行に加えて利用者が辿れる名前と URL を表示する。
    #[test]
    fn status_formatter_shows_incidents() {
        let out = render(&status_report(OfficialState::Degraded, false, true));
        assert!(out.contains("degraded"), "{out}");
        assert!(
            out.contains("Provider: Incident\n  https://stspg.io/i"),
            "{out}"
        );
    }

    /// 更新時刻は経過時間の表現をそのまま置く (`elapsed` が既に「いつ」を言い切る)。
    #[test]
    fn status_formatter_writes_the_age_once() {
        let out = render(&status_report(OfficialState::Operational, false, false));
        assert!(out.contains("just now"), "{out}");
        assert!(!out.contains("just now ago"), "{out}");
        assert!(!out.contains("ago ago"), "{out}");
    }

    /// stale は更新時刻の直後へ明示し、現在値と誤認させない。
    #[test]
    fn status_formatter_marks_stale_snapshots() {
        let out = render(&status_report(OfficialState::Operational, true, false));
        assert!(out.contains(" stale"), "{out}");
    }

    /// UPDATED 列も、ミリ秒の 2 つの時刻の差から組む。
    #[test]
    fn status_updated_column_reads_millisecond_stamps() {
        let mut report = status_report(OfficialState::Operational, false, false);
        report.generated_at = 1_785_326_400_000;
        report.services[0].official.observed_at = Some(1_785_326_400_000 - 7200 * SECOND);

        let out = render(&report);
        assert!(out.contains("2h ago"), "{out}");
    }
}
