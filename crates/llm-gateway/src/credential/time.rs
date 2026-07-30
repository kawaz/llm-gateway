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

/// その時刻を、この機械の地方時で見たときの日付 (`YYYY-MM-DD`)。
///
/// 使用量を日ごとに束ねる鍵に使う。UTC で数えると JST では日の境目が朝 9 時に
/// なり、「今日どれだけ使ったか」が体感とずれる (DR-0011)。
pub fn local_date(unix: i64) -> String {
    date_of(unix + local_offset(unix))
}

/// unix 秒を `YYYY-MM-DD` にする。渡すのは地方時へずらし終えた値。
fn date_of(shifted: i64) -> String {
    let (y, mo, d) = civil_from_days(shifted.div_euclid(86_400));
    format!("{y:04}-{mo:02}-{d:02}")
}

/// この機械の地方時が UTC からどれだけ離れているか (秒)。
///
/// Design rationale: 日時ライブラリを入れず libc の `localtime_r` を借りる。
/// offset は夏時間の有無と切り替え時刻で年内にも動くので、`TZ` と OS の
/// タイムゾーン表を読む相手に任せるしかない。std に口が無く、自前で
/// `/etc/localtime` (TZif) を解くのは、この 1 用途に対して重すぎる。
/// 時刻を渡して都度引くのは、常駐したまま夏時間の境目を跨いでも正しく
/// 転がるようにするため。
fn local_offset(unix: i64) -> i64 {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: 渡すのは有効な time_t 1 つと、この場で確保した tm 1 つ。
    // localtime_r は結果を渡した tm にだけ書き、共有の状態を触らない
    // (再入可能版を選ぶのはそのため)。失敗すれば null が返るだけ。
    let filled = unsafe { libc::localtime_r(&(unix as libc::time_t), &mut tm) };
    if filled.is_null() {
        // タイムゾーンを引けない環境では UTC として数える。日の境目はずれるが、
        // 集計そのものは続く。
        return 0;
    }
    tm.tm_gmtoff as i64
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

    /// 日付の切り出しは offset を足した後の値で行う。
    ///
    /// 実機の地方時に依らず確かめられるよう、ずらした値を直に渡す。
    #[test]
    fn date_is_cut_from_the_shifted_time() {
        // 2026-07-29T12:00:00Z
        let noon = 1_785_326_400;
        assert_eq!(date_of(noon), "2026-07-29");
        // JST (+9h) では同じ瞬間がまだ 29 日の 21 時。
        assert_eq!(date_of(noon + 9 * 3600), "2026-07-29");
        // UTC の 15 時を過ぎると JST は翌日に入る。
        assert_eq!(date_of(noon + 12 * 3600), "2026-07-30");
        // UTC の 0 時直前は、JST ではもう翌日。
        assert_eq!(date_of(noon - 12 * 3600 + 9 * 3600), "2026-07-29");
    }

    /// 年と月の境目でも桁が崩れない。
    #[test]
    fn date_pads_every_field() {
        let new_year = parse_rfc3339("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(date_of(new_year), "2026-01-01");
        assert_eq!(date_of(new_year - 1), "2025-12-31");
        assert_eq!(
            date_of(parse_rfc3339("2028-02-29T00:00:00Z").unwrap()),
            "2028-02-29"
        );
    }

    /// 地方時の日付が形として読める。
    ///
    /// 値そのものは実機のタイムゾーン次第なので、形と「UTC 日付との差は
    /// 高々 1 日」だけを見る。
    #[test]
    fn local_date_looks_like_a_date() {
        let now = now_unix();
        let local = local_date(now);
        let parts: Vec<&str> = local.split('-').collect();
        assert_eq!(parts.len(), 3, "{local}");
        assert_eq!(
            (parts[0].len(), parts[1].len(), parts[2].len()),
            (4, 2, 2),
            "{local}"
        );

        // どの地方時でも、UTC の前日・当日・翌日のどれか。
        let near: Vec<String> = [-86_400, 0, 86_400]
            .iter()
            .map(|d| date_of(now + d))
            .collect();
        assert!(near.contains(&local), "{local} が {near:?} に無い");
    }
}
