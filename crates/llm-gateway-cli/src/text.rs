//! 端末に並べるときの共通の道具 (桁・時間・色)。

use llm_gateway::credential::time::to_unix_secs;

/// 端末上の見た目の幅。日本語は 2 桁ぶん取る。
pub fn width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x20000..=0x3FFFD)
}

/// 3 桁ごとに区切る。トークン数は 6〜7 桁になるので、区切らないと読めない。
pub fn thousands(n: u64) -> String {
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

/// 経過した長さ (ミリ秒) を人が読む形に。JSON の時刻は Unix ミリ秒なので、
/// その差もミリ秒で来る。
pub fn elapsed_ms(ms: i64) -> String {
    elapsed(to_unix_secs(ms))
}

pub fn elapsed(secs: i64) -> String {
    match secs {
        s if s < 60 => "just now".to_owned(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// 残り時間を `3h01m` / `1d20h` のような形式にする。(`formatDuration` の移植)
///
/// `wide` なら先頭の単位を 2 桁にして `01d20h` / `00h32m` の 6 桁固定にする。
/// 複数 credential を縦に並べる表で、日〜分をまたいでも桁が波打たないように。
pub fn format_duration(secs: i64, wide: bool) -> String {
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
pub fn color_enabled() -> bool {
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

pub fn sgr_fg(n: u8, color: bool) -> String {
    sgr(&format!("38;5;{n}"), color)
}

pub fn sgr_bg(n: u8, color: bool) -> String {
    sgr(&format!("48;5;{n}"), color)
}

pub fn sgr_reset(color: bool) -> String {
    sgr("0", color)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i64 = 1_000;

    #[test]
    fn width_counts_japanese_as_two_columns() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("名前"), 4);
        assert_eq!(width("あと 16 分"), 4 + 6);
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

    #[test]
    fn elapsed_time_is_readable() {
        assert_eq!(elapsed(0), "just now");
        assert_eq!(elapsed(59), "just now");
        assert_eq!(elapsed(120), "2m ago");
        assert_eq!(elapsed(7200), "2h ago");
        assert_eq!(elapsed(86_400 * 2), "2d ago");
    }

    /// JSON は Unix ミリ秒で来るので、経過はミリ秒の差から組む。
    #[test]
    fn elapsed_reads_a_millisecond_gap() {
        assert_eq!(elapsed_ms(59 * SECOND), "just now");
        assert_eq!(elapsed_ms(120 * SECOND), "2m ago");
        assert_eq!(elapsed_ms(2 * 3600 * SECOND), "2h ago");
        assert_eq!(
            elapsed_ms(999),
            "just now",
            "1 秒に満たない差でも桁を取り違えない"
        );
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

    /// 色を出さないときは、印字そのものを空にする (幅を食わせない)。
    #[test]
    fn escape_codes_disappear_when_color_is_off() {
        assert_eq!(sgr_fg(40, false), "");
        assert_eq!(sgr_bg(24, false), "");
        assert_eq!(sgr_reset(false), "");
        assert_eq!(sgr_fg(40, true), "\x1b[38;5;40m");
        assert_eq!(sgr_bg(24, true), "\x1b[48;5;24m");
        assert_eq!(sgr_reset(true), "\x1b[0m");
    }
}
