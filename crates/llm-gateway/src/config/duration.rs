//! 設定に書く長さ (`30s` / `1h 30m` / `2d 6h`) の読み書き。
//!
//! 書き方は「数 + 単位」を並べたもので、並べた分を足し合わせる。
//!
//! - 数は 0 以上の整数か小数 (`1.5h`)。符号は書けない
//! - 単位は下の表のどれか。大文字小文字を区別する (`M` は月、`m` は分)
//! - 項と項の間、数と単位の間の空白は読み飛ばす (`1h30m` も `1h 30m` も `1 h` も読める)
//! - 単位を持たない書き方で読めるのは `0` だけ
//!
//! | 単位 | 書き方 |
//! |---|---|
//! | ナノ秒 | `nanos` `nsec` `ns` |
//! | マイクロ秒 | `usec` `us` `µs` |
//! | ミリ秒 | `millis` `msec` `ms` |
//! | 秒 | `seconds` `second` `secs` `sec` `s` |
//! | 分 | `minutes` `minute` `mins` `min` `m` |
//! | 時 | `hours` `hour` `hrs` `hr` `h` |
//! | 日 (24 時間) | `days` `day` `d` |
//! | 週 (7 日) | `weeks` `week` `wks` `wk` `w` |
//! | 月 (30.44 日) | `months` `month` `M` |
//! | 年 (365.25 日) | `years` `year` `yrs` `yr` `y` |
//!
//! Design rationale: この書き方は humantime 2.x の `parse_duration` /
//! `format_duration` と同じで、今ある設定ファイルをそのまま読むために揃えて
//! いる。humantime はメンテが終わったので自前で持つ。jiff の friendly 形式は
//! `0` と前後の空白、月・年 (`M` / `month` / `y`) を読めず、負の長さ
//! (`-5m` / `5m ago`) や ISO 8601 (`PT5M`) を読んでしまうため、包んで差を
//! 埋めるより書き方を直接持つ方が小さい。

use std::fmt;
use std::time::Duration;

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// 単位の名前と、その 1 単位のナノ秒。
const UNITS: &[(&[&str], u128)] = &[
    (&["nanos", "nsec", "ns"], 1),
    (&["usec", "us", "µs"], 1_000),
    (&["millis", "msec", "ms"], 1_000_000),
    (&["seconds", "second", "secs", "sec", "s"], SECOND),
    (&["minutes", "minute", "mins", "min", "m"], 60 * SECOND),
    (&["hours", "hour", "hrs", "hr", "h"], HOUR),
    (&["days", "day", "d"], DAY),
    (&["weeks", "week", "wks", "wk", "w"], 7 * DAY),
    (&["months", "month", "M"], MONTH),
    (&["years", "year", "yrs", "yr", "y"], YEAR),
];

const SECOND: u128 = NANOS_PER_SEC;
const HOUR: u128 = 3600 * SECOND;
const DAY: u128 = 24 * HOUR;
/// 30.44 日。
const MONTH: u128 = 2_630_016 * SECOND;
/// 365.25 日。
const YEAR: u128 = 31_557_600 * SECOND;

/// 単位の文字。英字と `µ` (U+00B5)。
fn is_unit_char(c: char) -> bool {
    c.is_ascii_alphabetic() || c == 'µ'
}

/// 設定に書かれた長さを読む。書き方はモジュールの説明のとおり。
pub fn parse(text: &str) -> Result<Duration, String> {
    let hint = "write a number followed by a unit, such as `30s`, `5m`, `1h 30m` or `2d`";
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    let mut chars = text.chars().peekable();
    let mut total: u128 = 0;
    let mut read_any = false;
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else { break };
        if !first.is_ascii_digit() {
            return Err(format!(
                "`{text}` is not a duration: `{first}` where a number is expected; {hint}"
            ));
        }

        // 整数部。数字の間の空白は読み飛ばす。
        let mut whole: u128 = 0;
        while let Some(c) = chars.next_if(|c| c.is_ascii_digit() || c.is_whitespace()) {
            if let Some(digit) = c.to_digit(10) {
                whole = whole
                    .checked_mul(10)
                    .and_then(|n| n.checked_add(u128::from(digit)))
                    .ok_or_else(|| format!("`{text}` is too long to be a duration"))?;
            }
        }

        // 小数部 (分子 / 分母)。
        let mut fraction: Option<(u128, u128)> = None;
        if chars.next_if_eq(&'.').is_some() {
            let (mut numerator, mut denominator) = (0u128, 1u128);
            while let Some(c) = chars.next_if(|c| c.is_ascii_digit() || c.is_whitespace()) {
                if let Some(digit) = c.to_digit(10) {
                    numerator = numerator
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(u128::from(digit)))
                        .ok_or_else(|| format!("`{text}` has too many decimal places"))?;
                    denominator = denominator
                        .checked_mul(10)
                        .ok_or_else(|| format!("`{text}` has too many decimal places"))?;
                }
            }
            if denominator == 1 {
                return Err(format!(
                    "`{text}` has a `.` without digits after it; {hint}"
                ));
            }
            fraction = Some((numerator, denominator));
        }

        let mut unit = String::new();
        while let Some(c) = chars.next_if(|&c| is_unit_char(c)) {
            unit.push(c);
        }
        // 単位の直後に来てよいのは、次の項の数字か空白か終わりだけ。
        if let Some(&c) = chars.peek()
            && !(c.is_ascii_digit() || c.is_whitespace())
        {
            return Err(format!(
                "`{text}` is not a duration: unexpected `{c}`; {hint}"
            ));
        }
        if unit.is_empty() {
            return Err(format!("`{text}` has a number without a unit; {hint}"));
        }
        let Some(&(_, scale)) = UNITS
            .iter()
            .find(|(names, _)| names.contains(&unit.as_str()))
        else {
            return Err(format!(
                "`{text}` has an unknown unit `{unit}`; use ns, us, ms, s, m, h, d, w, M (months) or y (years)"
            ));
        };

        let overflow = || format!("`{text}` is too long to be a duration");
        let mut nanos = whole.checked_mul(scale).ok_or_else(overflow)?;
        if let Some((numerator, denominator)) = fraction {
            let scaled = numerator.checked_mul(scale).ok_or_else(overflow)?;
            if scaled % denominator != 0 {
                return Err(format!("`{text}` is finer than a nanosecond"));
            }
            nanos = nanos
                .checked_add(scaled / denominator)
                .ok_or_else(overflow)?;
        }
        total = total.checked_add(nanos).ok_or_else(overflow)?;
        read_any = true;
    }
    if !read_any {
        return Err(format!("the duration is empty; {hint}"));
    }
    let secs = u64::try_from(total / NANOS_PER_SEC)
        .map_err(|_| format!("`{text}` is too long to be a duration"))?;
    let subsec = u32::try_from(total % NANOS_PER_SEC).expect("below one second");
    Ok(Duration::new(secs, subsec))
}

/// 長さを [`parse`] で読み戻せる書き方にする。
///
/// 大きい単位から順に 0 でない項だけを空白で区切って並べる (`2h 37m`)。
/// 年・月・日は 2 以上で複数形にする (`1day`、`2days`)。0 は `0s`。
pub fn format(value: Duration) -> Formatted {
    Formatted(value)
}

/// [`format`] の結果。`Display` で書き出す。
#[derive(Debug, Clone, Copy)]
pub struct Formatted(Duration);

impl fmt::Display for Formatted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        let nanos = self.0.subsec_nanos();
        if secs == 0 && nanos == 0 {
            return f.write_str("0s");
        }
        const YEAR_SECS: u64 = 31_557_600;
        const MONTH_SECS: u64 = 2_630_016;
        let in_year = secs % YEAR_SECS;
        let in_month = in_year % MONTH_SECS;
        let in_day = in_month % 86_400;
        let parts: [(u64, &str, bool); 9] = [
            (secs / YEAR_SECS, "year", true),
            (in_year / MONTH_SECS, "month", true),
            (in_month / 86_400, "day", true),
            (in_day / 3600, "h", false),
            (in_day % 3600 / 60, "m", false),
            (in_day % 60, "s", false),
            (u64::from(nanos / 1_000_000), "ms", false),
            (u64::from(nanos / 1000 % 1000), "us", false),
            (u64::from(nanos % 1000), "ns", false),
        ];
        let mut started = false;
        for (value, name, plural) in parts {
            if value == 0 {
                continue;
            }
            if started {
                f.write_str(" ")?;
            }
            write!(f, "{value}{name}")?;
            if plural && value > 1 {
                f.write_str("s")?;
            }
            started = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// 今の設定ファイルと MANUAL / 設定例に出てくる書き方がそのまま読める。
    /// ここが崩れると、既にある設定ファイルが起動時に読めなくなる。
    #[test]
    fn reads_the_forms_written_in_existing_configs() {
        for (text, want) in [
            // [dump] / [status] の既定と MANUAL の例。
            ("5m", secs(5 * 60)),
            ("10m", secs(10 * 60)),
            ("24h", secs(24 * 3600)),
            ("48h", secs(48 * 3600)),
            ("30s", secs(30)),
            // spend_down_within / pace_cap.step の例。
            ("40h", secs(40 * 3600)),
            ("12h", secs(12 * 3600)),
            ("1d", secs(86_400)),
            ("90m", secs(90 * 60)),
            ("2d", secs(2 * 86_400)),
            // 複合表記は並べた分を足す。区切りの空白は有っても無くてもよい。
            ("1h 30m", secs(5400)),
            ("1h30m", secs(5400)),
            ("2d 6h", secs((2 * 24 + 6) * 3600)),
            ("1hour 12min 5s", secs(3600 + 12 * 60 + 5)),
            // 数と単位の間、前後の空白も読み飛ばす。
            ("10 m", secs(600)),
            (" 5m ", secs(300)),
            // 0 だけは単位なしで書ける。
            ("0", Duration::ZERO),
            ("0s", Duration::ZERO),
        ] {
            assert_eq!(parse(text), Ok(want), "{text:?}");
        }
    }

    /// 単位の綴りは表のとおりで、週・月・年は日数の固定倍として読む
    /// (月 = 30.44 日、年 = 365.25 日。暦には合わせない)。大文字の `M` は月、
    /// 小文字の `m` は分。
    #[test]
    fn reads_every_unit_spelling() {
        for (names, unit) in UNITS {
            for name in *names {
                let got = parse(&format!("2{name}")).unwrap();
                assert_eq!(got.as_nanos(), 2 * unit, "{name}");
            }
        }
        assert_eq!(parse("1w"), Ok(secs(7 * 86_400)));
        assert_eq!(parse("1M"), Ok(secs(2_630_016)));
        assert_eq!(parse("1y"), Ok(secs(31_557_600)));
        assert_eq!(parse("1m"), Ok(secs(60)));
    }

    /// 小数は単位に掛けて読む。ナノ秒より細かくなる値は表せないので断る
    /// (丸めると書いた値と違う長さで動く)。
    #[test]
    fn reads_fractions_down_to_a_nanosecond() {
        assert_eq!(parse("1.5h"), Ok(secs(5400)));
        assert_eq!(parse("4.2s"), Ok(Duration::new(4, 200_000_000)));
        assert_eq!(parse("0.000000001s"), Ok(Duration::from_nanos(1)));
        assert!(parse("0.5ns").is_err());
        // `.` の後に数字が無いのは書き間違い。
        assert!(parse("1.h").is_err());
        assert!(parse("1.").is_err());
    }

    /// 長さとして読めない書き方は、設定を読んだ時点で断る。
    /// 黙って 0 や既定値にすると、書いた人の意図と違う長さで動く。
    #[test]
    fn rejects_what_is_not_a_duration() {
        for text in [
            // 空。
            "",
            " ",
            // 単位が無い (0 以外)。秒か分かを推測しない。
            "5",
            "40",
            // 知らない単位。大文字の H / D は時・日ではない。
            "12H",
            "1D",
            "5x",
            // 符号・区切り記号・後置の語は書き方に無い。負の長さは無い。
            "-5m",
            "+5m",
            "1h,30m",
            "5m ago",
            "PT5M",
            // 単位の途中で空白を挟むと別の語になる。
            "2 hours min",
            "1m in",
            // 割合は WindowSpan 側が読む。ここでは長さでない。
            "25%",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }

    /// 書き出しは大きい単位から 0 でない項を空白区切りで並べる。年・月・日は
    /// 2 以上で複数形。この形は検証の失敗メッセージにそのまま出る。
    #[test]
    fn formats_from_the_largest_unit() {
        for (value, want) in [
            (Duration::ZERO, "0s"),
            (secs(30), "30s"),
            (secs(5 * 60), "5m"),
            (secs(9420), "2h 37m"),
            (secs(24 * 3600), "1day"),
            (secs(48 * 3600), "2days"),
            (secs(54 * 3600), "2days 6h"),
            (secs(2_630_016), "1month"),
            (secs(31_557_600 + 2 * 2_630_016), "1year 2months"),
            (Duration::from_millis(32), "32ms"),
            (Duration::new(1, 2_003_004), "1s 2ms 3us 4ns"),
        ] {
            assert_eq!(format(value).to_string(), want, "{value:?}");
        }
    }

    /// 書き出した文字列は読み戻すと同じ長さになる。設定を書き出して
    /// 読み直しても値が変わらないための約束。
    #[test]
    fn formatted_text_reads_back_to_the_same_value() {
        for value in [
            Duration::ZERO,
            secs(1),
            secs(59),
            secs(3661),
            secs(86_399),
            secs(90 * 86_400),
            secs(400 * 86_400),
            Duration::new(31_557_600 * 3 + 12_345, 678_901_234),
        ] {
            let text = format(value).to_string();
            assert_eq!(parse(&text), Ok(value), "{text}");
        }
    }
}
