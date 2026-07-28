//! 認証情報が持つ時刻の読み書き。
//!
//! 保存する時刻は RFC 3339 で揃えてある (`expired` / `last_refresh` /
//! `denied_beta` の値)。日時ライブラリを足すほどの用途ではないので、
//! 必要な形 (`2026-07-28T02:54:00+09:00`) だけ解釈する。

/// RFC 3339 を unix 秒にする。読めなければ `None`。
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |from: usize, to: usize| s.get(from..to)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);

    let offset = match b.get(19) {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let oh = num(20, 22)?;
            let om = num(23, 25)?;
            let secs = oh * 3600 + om * 60;
            if sign == b'-' { -secs } else { secs }
        }
        // 小数秒付き (`…:00.123Z`) は今のところ出てこない。
        Some(_) => return None,
    };

    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// unix 秒を RFC 3339 にする。
pub fn format_rfc3339(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// 今この瞬間の unix 秒。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Howard Hinnant の civil_from_days / days_from_civil。
/// 1970-01-01 からの日数と暦日を相互変換する。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-27T19:00:00Z
    const NOW: i64 = 1_785_178_800;

    #[test]
    fn parses_offsets() {
        // 同じ時刻を別の書き方で表したもの。
        let utc = parse_rfc3339("2026-07-27T19:00:00Z").unwrap();
        assert_eq!(parse_rfc3339("2026-07-28T04:00:00+09:00"), Some(utc));
        assert_eq!(parse_rfc3339("2026-07-27T14:00:00-05:00"), Some(utc));
        assert_eq!(parse_rfc3339("2026-07-27T19:00:00"), Some(utc));
    }

    /// 実運用の auth JSON にある値をそのまま読めるか。
    #[test]
    fn parses_real_expiry_values() {
        assert!(parse_rfc3339("2026-07-28T02:54:00+09:00").is_some());
        assert!(parse_rfc3339("2026-08-02T10:08:18+09:00").is_some());
    }

    #[test]
    fn rejects_unreadable_values() {
        for bad in [
            "",
            "not-a-date",
            "2026-07",
            "yesterday",
            "2026-07-28T02:54:00.5Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn round_trips_through_format() {
        for t in [0, NOW, NOW + 28_800, 253_402_300_799] {
            assert_eq!(parse_rfc3339(&format_rfc3339(t)), Some(t), "t={t}");
        }
    }

    #[test]
    fn handles_leap_day() {
        let feb29 = parse_rfc3339("2028-02-29T12:00:00Z").unwrap();
        assert_eq!(format_rfc3339(feb29), "2028-02-29T12:00:00Z");
    }

    /// 実時刻が取れる (0 のまま返らない)。
    #[test]
    fn now_is_after_the_epoch() {
        assert!(now_unix() > 1_700_000_000);
    }
}
