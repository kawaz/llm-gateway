//! 認証情報ごとの利用量 (DR-0007)。
//!
//! スナップショットを持っているのは走っている側なので、ここは聞いて整形する
//! だけ。表示は claude-statusline の 5h/7d バー (`dualBar` / `dualInfo`) を
//! 移植したもので、1 credential 1 行に並べる。

use std::process::ExitCode;

use llm_gateway::credential::time::{format_rfc3339_ms, to_unix_secs};
use llm_gateway::daemon::registry::Registry;
use llm_gateway::quota::{CredentialUsage, Report, Window};

use crate::destination::{self, Target};
use crate::failure::Failure;
use crate::help;
use crate::options::{split, take_value};
use crate::text::{color_enabled, elapsed_ms, format_duration, sgr_bg, sgr_fg, sgr_reset, width};

/// `usage` に渡された内容。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    refresh: bool,
    unit: Option<String>,
}

pub fn run(args: &[String]) -> Result<ExitCode, Failure> {
    if help::wanted(args) {
        print!("{}", help::TOP);
        return Ok(ExitCode::SUCCESS);
    }
    let parsed = parse(args)?;
    let target = destination::resolve(&Registry::open(), parsed.unit.as_deref())?;
    let report: Report = ask(&target, parsed.refresh)?;
    print!("{}", render(&report));
    Ok(ExitCode::SUCCESS)
}

fn ask(target: &Target, refresh: bool) -> Result<Report, Failure> {
    destination::ask(target, "usage", if refresh { "?refresh=true" } else { "" })
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

fn render(report: &Report) -> String {
    let now_ms = report.generated_at;
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

        match c.auth.as_ref().map(|auth| &auth.status) {
            Some(llm_gateway::quota::AuthStatus::ReloginRequired) => {
                let reason = c
                    .auth
                    .as_ref()
                    .and_then(|auth| auth.reason.as_deref())
                    .unwrap_or("token rejected; log in again");
                out.push_str(&format!("token rejected — relogin required: {reason}"));
            }
            Some(llm_gateway::quota::AuthStatus::OrgNotAllowed) => {
                // 名乗るのは観測した事実だけ。原因の推定は hint として続ける。
                let hint = c
                    .auth
                    .as_ref()
                    .and_then(|auth| auth.hint.as_deref().or(auth.reason.as_deref()))
                    .unwrap_or("the upstream refuses OAuth use for this organization");
                out.push_str(&format!("(org not allowed) {hint}"));
            }
            Some(llm_gateway::quota::AuthStatus::Degraded) => {
                let reason = c
                    .auth
                    .as_ref()
                    .and_then(|auth| auth.reason.as_deref())
                    .unwrap_or("refresh failed");
                out.push_str(&format!("refresh failing (transient): {reason}"));
            }
            _ => match &c.snapshot {
                Some(s) => {
                    out.push_str(&window_field(
                        '⏰',
                        s.five_hour.as_ref(),
                        now_ms,
                        5 * 3600,
                        color,
                    ));
                    out.push(' ');
                    out.push_str(&window_field(
                        '📆',
                        s.seven_day.as_ref(),
                        now_ms,
                        7 * 86_400,
                        color,
                    ));
                    let age_ms = now_ms - s.observed_at;
                    if age_ms > 300_000 {
                        out.push_str(&format!(" ({})", elapsed_ms(age_ms)));
                    }
                }
                None => out.push_str(match c.support {
                    llm_gateway::quota::Support::Unobserved => "unobserved",
                    llm_gateway::quota::Support::NotApplicable => "not_applicable",
                    llm_gateway::quota::Support::UpstreamDependent => "upstream_dependent",
                    llm_gateway::quota::Support::Observed => "-",
                }),
            },
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

/// 1 つの窓 (5h/7d) の表示。使用率とウィンドウ経過率を `dualBar` で重ねて
/// 描き、続けて `使用率%/経過率%/残り時間` を出す。取れていなければ `-`。
fn window_field(
    icon: char,
    window: Option<&Window>,
    now_ms: i64,
    window_secs: i64,
    color: bool,
) -> String {
    let (Some(util), Some(reset_ms)) = (
        window.and_then(|w| w.utilization),
        window.and_then(|w| w.reset),
    ) else {
        return format!("{icon}-");
    };
    let util_pct = util * 100.0;
    let elapsed_pct = calc_elapsed(reset_ms, window_secs * 1000, now_ms);
    let bar = dual_bar(util_pct, elapsed_pct, 10, color);
    // 7d 窓は日〜分をまたいで幅が揺れるので、先頭単位を 2 桁にして 6 桁固定にする。
    // 5h 窓は 10 時間に届かず 5 桁で揃うので、そのまま。
    let wide = window_secs >= 86_400;
    let remaining = format_duration(to_unix_secs(reset_ms - now_ms), wide);
    let info = dual_info(util_pct, elapsed_pct, &remaining, color);
    let expired = if window.is_some_and(|window| window.expired) {
        " (expired)"
    } else {
        ""
    };
    format!("{icon}{bar}{info}{expired}")
}

/// リセット時刻とウィンドウ長から、窓の経過率 (0〜100) を逆算する。
/// (claude-statusline `calcElapsed` の移植)
fn calc_elapsed(reset_ms: i64, window_ms: i64, now_ms: i64) -> f64 {
    if window_ms <= 0 {
        return 0.0;
    }
    let window_start = reset_ms - window_ms;
    let pct = (now_ms - window_start) as f64 / window_ms as f64 * 100.0;
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

/// 表に収まらないもの (上限到達の警告・プローブの失敗)。
///
/// overage (従量課金フォールバックの可否) と window の status はここに出さない。
/// どちらも使用率の % を見れば足りる情報の重複で、毎回並ぶとノイズになる
/// (kawaz 裁定)。JSON には残っているので、機械で読む分には失われない。
fn remarks(c: &CredentialUsage) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(e) = &c.probe_error {
        lines.push(format!("{}: could not read it again ({e})", c.name));
    }
    // モデル別の枠は上の行に出せない (5 時間 / 7 日の欄しかない) うえ、
    // 応答ヘッダにも現れないので、聞けたときはここに並べる。
    for limit in c.limits.iter().flatten() {
        let Some(model) = &limit.model else {
            continue;
        };
        let mut line = format!("{}: the {model} limit is at {:.0}%", c.name, limit.percent);
        if let Some(at_ms) = limit.resets_at {
            line.push_str(&format!(" (resets at {})", format_rfc3339_ms(at_ms)));
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn parse_args(list: &[&str]) -> Result<Args, Failure> {
        parse(&args(list))
    }

    /// 宛先は名前で指す。省略時は登録簿が決める (ここでは何も決めない)。
    #[test]
    fn usage_takes_refresh_and_a_unit() {
        let plain = parse_args(&[]).unwrap();
        assert!(
            !plain.refresh,
            "not fired by default (does not consume usage)"
        );
        assert_eq!(plain.unit, None);

        let refreshing = parse_args(&["--refresh", "--unit", "unstable"]).unwrap();
        assert!(refreshing.refresh);
        assert_eq!(refreshing.unit.as_deref(), Some("unstable"));

        assert_eq!(
            parse_args(&["--unit=stable"]).unwrap().unit.as_deref(),
            Some("stable")
        );
    }

    /// 設定ファイルは渡さない。宛先を覚えるのは登録簿の役目 (DR-0028 決定 6)。
    #[test]
    fn usage_no_longer_takes_a_configuration_file() {
        let e = parse_args(&["--config", "/tmp/c.toml"]).unwrap_err();
        assert!(e.message().contains("--config"), "{e:?}");
    }

    #[test]
    fn usage_rejects_unknown_options() {
        let e = parse_args(&["--refesh"]).unwrap_err();
        assert!(e.message().contains("--refesh"), "{e:?}");
        assert!(parse_args(&["--unit"]).is_err(), "a name is required");
    }

    /// 2026-07-29T12:00:00Z を Unix ミリ秒で。JSON の時刻はこの数え方 (DR-0012)。
    const NOW: i64 = 1_785_326_400_000;
    const SECOND: i64 = 1_000;

    fn window(utilization: f64, reset: i64, status: &str) -> Window {
        Window {
            utilization: Some(utilization),
            status: Some(status.to_owned()),
            reset: Some(reset),
            window_seconds: None,
            expired: false,
        }
    }

    fn observed() -> CredentialUsage {
        CredentialUsage::new(
            "claude-personal",
            "claude_oauth",
            llm_gateway::quota::Support::Observed,
            Some(llm_gateway::quota::Snapshot {
                observed_at: NOW - 120 * SECOND,
                five_hour: Some(window(0.71, NOW + 960 * SECOND, "allowed")),
                seven_day: Some(window(0.3, NOW + 86_400 * 4 * SECOND, "allowed")),
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

        assert!(out.contains("71%/95%/0h16m"), "5h window:\n{out}");
        assert!(out.contains("30%/43%/04d00h"), "7d window:\n{out}");
        assert!(
            !out.contains("ago"),
            "just fetched, so no staleness is shown:\n{out}"
        );
    }

    /// 再認可が必要な credential は古い quota bar より auth 異常を優先し、実行可能な login 指示を出す。
    #[test]
    fn relogin_required_replaces_the_quota_bars() {
        let mut credential = observed();
        credential.auth = Some(llm_gateway::quota::AuthState {
            status: llm_gateway::quota::AuthStatus::ReloginRequired,
            reason: Some("run `llm-gateway login --type claude_oauth claude-personal`".to_owned()),
            hint: None,
            login_path: None,
            observed_at: NOW,
        });
        let out = render(&report(vec![credential]));
        assert!(out.contains("token rejected — relogin required"), "{out}");
        assert!(
            out.contains("llm-gateway login --type claude_oauth claude-personal"),
            "{out}"
        );
        assert!(
            !out.contains("71%"),
            "stale quota must not hide the auth failure: {out}"
        );
    }

    /// 組織ごとの断りは期限切れでも再認可でもない。観測した事実を名乗り、
    /// 原因の推定は hint として添える。
    #[test]
    fn an_org_refusal_is_not_shown_as_expired() {
        let mut credential = observed();
        credential.auth = Some(llm_gateway::quota::AuthState {
            status: llm_gateway::quota::AuthStatus::OrgNotAllowed,
            reason: Some("the login still works, but the upstream refuses it".to_owned()),
            hint: Some("an inactive subscription is one cause; check the account".to_owned()),
            login_path: None,
            observed_at: NOW,
        });
        let out = render(&report(vec![credential]));
        assert!(out.contains("(org not allowed)"), "{out}");
        assert!(
            out.contains("one cause"),
            "the guess is offered, not asserted: {out}"
        );
        assert!(!out.contains("expired"), "{out}");
        assert!(
            !out.contains("relogin required"),
            "logging in again does not lift an organization-wide refusal: {out}"
        );
    }

    /// 一時的な refresh 失敗は再認可を促さず、再試行可能な degraded 状態として区別する。
    #[test]
    fn degraded_auth_is_rendered_as_transient() {
        let mut credential = observed();
        credential.auth = Some(llm_gateway::quota::AuthState {
            status: llm_gateway::quota::AuthStatus::Degraded,
            reason: Some("the refresh endpoint returned 503".to_owned()),
            hint: None,
            login_path: None,
            observed_at: NOW,
        });
        let out = render(&report(vec![credential]));
        assert!(out.contains("refresh failing (transient)"), "{out}");
        assert!(!out.contains("relogin required"), "{out}");
    }

    /// reset を跨いだ window は保存済み利用率を表示しても、現在値ではないことを同じ欄で明示する。
    #[test]
    fn expired_window_is_marked_next_to_its_percentage() {
        let mut credential = observed();
        credential
            .snapshot
            .as_mut()
            .unwrap()
            .five_hour
            .as_mut()
            .unwrap()
            .expired = true;
        let out = render(&report(vec![credential]));
        assert!(out.contains("(expired)"), "{out}");
        assert!(
            out.contains("71%"),
            "the historical observation remains visible: {out}"
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
                resets_at: llm_gateway::credential::time::parse_rfc3339_ms(
                    "2026-08-02T08:59:59.571539+00:00",
                ),
                model: None,
                model_id: None,
                window_seconds: Some(7 * 24 * 60 * 60),
                is_active: true,
            },
            llm_gateway::quota::QuotaLimit {
                kind: "weekly_scoped".to_owned(),
                percent: 80.0,
                severity: Some("warning".to_owned()),
                resets_at: llm_gateway::credential::time::parse_rfc3339_ms(
                    "2026-08-02T08:59:59.571875+00:00",
                ),
                model: Some("Fable".to_owned()),
                model_id: None,
                window_seconds: Some(7 * 24 * 60 * 60),
                is_active: false,
            },
        ]);

        let out = render(&report(vec![c]));
        assert!(out.contains("the Fable limit is at 80%"), "{out}");
        assert!(
            out.contains("resets at 2026-08-02T08:59:59Z"),
            "the millisecond stamp is shown to the second:\n{out}"
        );
        assert!(
            !out.contains("weekly_all"),
            "a model-less limit is omitted since it overlaps the table column:\n{out}"
        );
    }

    /// 取得から 5 分を超えたスナップショットだけ古さを添える。
    #[test]
    fn shows_age_only_when_snapshot_is_stale() {
        let mut c = observed();
        if let Some(s) = c.snapshot.as_mut() {
            s.observed_at = NOW - 400 * SECOND;
        }
        let out = render(&report(vec![c]));
        assert!(out.contains("(6m ago)"), "{out}");
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
            s.observed_at = NOW - 3 * 3600 * SECOND;
        }
        let out = render(&report(vec![c]));
        assert!(out.contains("(3h ago)"), "{out}");
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
        broken.probe_error = Some("connection refused".to_owned());

        let out = render(&report(vec![hit, broken]));
        assert!(!out.contains("rejected"), "{out}");
        assert!(!out.contains("out_of_credits"), "{out}");
        assert!(out.contains("nowhere: could not read it again"), "{out}");
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
        assert!(!out.contains("token"), "{out}");
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
}
