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

    // 小数秒 (`…:59.571539+00:00`) は秒より細かいので捨てる。枠の開く時刻は
    // マイクロ秒まで返ってくるが、こちらが持つのは秒。
    let zone = match b.get(19) {
        Some(b'.') => 20 + b[20..].iter().take_while(|c| c.is_ascii_digit()).count(),
        _ => 19,
    };

    let offset = match b.get(zone) {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let oh = num(zone + 1, zone + 3)?;
            let om = num(zone + 4, zone + 6)?;
            let secs = oh * 3600 + om * 60;
            if sign == b'-' { -secs } else { secs }
        }
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
    date_at_offset(unix, local_offset(unix))
}

/// UTC から `offset` 秒ずれた地方時で見た日付 (`YYYY-MM-DD`)。
///
/// offset を引数で受けるのは、機械のタイムゾーンに依らず試験できるようにする
/// ため。OS を引く部分は [`local_offset`] に寄せてある。
fn date_at_offset(unix: i64, offset: i64) -> String {
    let (y, mo, d) = civil_from_days((unix + offset).div_euclid(86_400));
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
///
/// `tm_gmtoff` は POSIX ではなく BSD / glibc の拡張。このリポの対象は macOS
/// (DR-0010)、CI も含めて Linux までなので、どちらにもある。
fn local_offset(unix: i64) -> i64 {
    // libc crate は `tzset` を Windows 向けにしか宣言していないので、こちらで
    // 引く。POSIX の関数なので macOS / Linux では libc に必ずある。
    unsafe extern "C" {
        fn tzset();
    }

    // タイムゾーンの読み込みを先に 1 回だけ済ませる。
    //
    // `localtime_r` は POSIX 上 tzname を設定する義務が無く、tz 状態の初期化を
    // いつ行うかは実装任せ。ここで明示的に 1 回通しておけば、以降の
    // `localtime_r` は**初期化済みの状態を読むだけ**になり、初回の初期化が
    // 複数スレッドから同時に走る形を考えなくてよくなる。
    static TZ_READY: std::sync::Once = std::sync::Once::new();
    TZ_READY.call_once(|| {
        // SAFETY: `tzset` は引数も戻り値も無く、プロセスのタイムゾーン状態を
        // `TZ` (と OS のタイムゾーン表) から読み直すだけ。`Once` で包んで
        // あるので同時に走らず、終わるまで後続のスレッドは待つ。
        unsafe { tzset() };
    });

    // SAFETY: `libc::tm` は repr(C) の整数の並びと `tm_zone` (生ポインタ) だけで
    // でき、どのフィールドも全 0 が有効な値 (生ポインタは null が有効)。
    // 不正な値の型を作らないので、ゼロ埋めで初期化できる。この後
    // `localtime_r` が全フィールドを埋めるので、読むのは埋まった後だけ。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };

    // SAFETY: 第 1 引数は有効な `time_t` 1 つへの参照、第 2 引数はこの場で
    // 初期化した `tm` への排他参照で、どちらも呼び出しの間だけ生きていれば
    // 足りる (localtime_r は参照を保持しない)。
    //
    // `localtime_r` は**スレッドセーフ**: `localtime` が共有の static に書くのに
    // 対し、こちらは渡した `tm` にだけ結果を書く (POSIX が再入可能版として
    // 規定するのがこの違い)。tz の初期化は上の `tzset` で済ませてあるので、
    // ここで触るのは渡した `tm` と、読み取り専用になった tz 状態だけ。
    // tokio の worker から同時に呼ばれても互いを壊さない。
    //
    // 残る前提は 1 つ、**`TZ` 環境変数を書き換える者がいないこと**。書き換えと
    // 同時に読むとデータ競合になる。この crate は `std::env::set_var` を呼ばず
    // (edition 2024 ではそれ自体が unsafe)、依存も起動後に環境を書き換えない。
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
        for bad in ["", "not-a-date", "2026-07", "yesterday"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    /// 枠の開く時刻はマイクロ秒まで返ってくる (DR-0007)。秒より細かい分は捨てる。
    #[test]
    fn fractional_seconds_are_dropped() {
        let utc = parse_rfc3339("2026-08-02T08:59:59Z").unwrap();
        assert_eq!(parse_rfc3339("2026-08-02T08:59:59.571539+00:00"), Some(utc));
        assert_eq!(parse_rfc3339("2026-08-02T08:59:59.5Z"), Some(utc));
        assert_eq!(parse_rfc3339("2026-08-02T17:59:59.571539+09:00"), Some(utc));
        assert_eq!(parse_rfc3339("2026-08-02T08:59:59.571539"), Some(utc));
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

    /// 東にずれた地方時 (offset が正) の日付。
    ///
    /// UTC の午後は、東側では既に翌日に入っていることがある。
    #[test]
    fn a_positive_offset_can_land_on_the_next_day() {
        // 2026-07-29T12:00:00Z
        let noon = 1_785_326_400;
        assert_eq!(date_at_offset(noon, 0), "2026-07-29", "UTC");
        // JST (+9h) ではまだ 29 日の 21 時。
        assert_eq!(date_at_offset(noon, 9 * 3600), "2026-07-29");
        // UTC の 15 時を過ぎると JST は翌日。
        assert_eq!(date_at_offset(noon + 3 * 3600, 9 * 3600), "2026-07-30");
        // キリバス (+14h) は最も東。UTC の 10 時で既に翌日。
        assert_eq!(date_at_offset(noon, 14 * 3600), "2026-07-30");
    }

    /// 西にずれた地方時 (offset が負) の日付。
    ///
    /// UTC の午前は、西側ではまだ前日。ここを間違えると、負の offset の下で
    /// 日付が 1 日ずれたまま集計される。
    #[test]
    fn a_negative_offset_can_land_on_the_previous_day() {
        // 2026-07-29T02:00:00Z
        let early = parse_rfc3339("2026-07-29T02:00:00Z").unwrap();
        assert_eq!(date_at_offset(early, 0), "2026-07-29", "UTC");
        // ニューヨーク (-4h、夏時間) ではまだ 28 日の 22 時。
        assert_eq!(date_at_offset(early, -4 * 3600), "2026-07-28");
        // ロサンゼルス (-7h) も前日。
        assert_eq!(date_at_offset(early, -7 * 3600), "2026-07-28");
        // 同じ日の午後なら、西側でも同じ日。
        let afternoon = parse_rfc3339("2026-07-29T20:00:00Z").unwrap();
        assert_eq!(date_at_offset(afternoon, -7 * 3600), "2026-07-29");
    }

    /// 日の境目のちょうど両側。
    ///
    /// 地方時の 00:00:00 は新しい日で、その 1 秒前は前の日。境界が 1 秒
    /// ずれると、深夜の利用が隣の日に付く。
    #[test]
    fn the_day_boundary_falls_between_the_right_two_seconds() {
        let offset = 9 * 3600;
        // 地方時で 2026-07-30T00:00:00 になる瞬間 (= UTC の 07-29T15:00:00)。
        let midnight = parse_rfc3339("2026-07-29T15:00:00Z").unwrap();

        assert_eq!(
            date_at_offset(midnight - 1, offset),
            "2026-07-29",
            "1 second before"
        );
        assert_eq!(date_at_offset(midnight, offset), "2026-07-30", "exactly at");
        assert_eq!(
            date_at_offset(midnight + 1, offset),
            "2026-07-30",
            "1 second after"
        );
    }

    /// epoch より前 (offset で 1970 年より手前に出る) でも崩れない。
    ///
    /// 負の日数になるので、切り捨ての向きを間違えると 1 日ずれる。
    #[test]
    fn dates_before_the_epoch_still_work() {
        let epoch = 0;
        assert_eq!(date_at_offset(epoch, 0), "1970-01-01");
        // 西にずれると epoch の瞬間はまだ 1969 年。
        assert_eq!(date_at_offset(epoch, -1), "1969-12-31");
        assert_eq!(date_at_offset(epoch, -11 * 3600), "1969-12-31");
    }

    /// 年と月の境目でも桁が崩れない。
    #[test]
    fn date_pads_every_field() {
        let new_year = parse_rfc3339("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(date_at_offset(new_year, 0), "2026-01-01");
        assert_eq!(date_at_offset(new_year, -1), "2025-12-31");
        assert_eq!(
            date_at_offset(parse_rfc3339("2028-02-29T00:00:00Z").unwrap(), 0),
            "2028-02-29",
            "leap day"
        );
    }

    /// 実機の offset は現実的な範囲に収まる。
    ///
    /// 世界の地方時は UTC-12 〜 UTC+14 の間。ここを外れる値が返るなら
    /// `tm_gmtoff` の読み方を間違えている。
    #[test]
    fn the_machine_offset_is_a_real_timezone() {
        let offset = local_offset(now_unix());
        assert!(
            (-12 * 3600..=14 * 3600).contains(&offset),
            "offset is outside the range of local timezones: {offset}"
        );
        // 15 分刻みでないタイムゾーンは存在しない。
        assert_eq!(offset % 900, 0, "not a multiple of 15 minutes: {offset}");
    }

    /// 地方時の日付が形として読める。
    ///
    /// 値そのものは実機のタイムゾーン次第なので、形と「実機の offset で
    /// 計算した値と一致する」ことを見る。
    #[test]
    fn local_date_uses_the_machine_offset() {
        let now = now_unix();
        let local = local_date(now);

        let parts: Vec<&str> = local.split('-').collect();
        assert_eq!(parts.len(), 3, "{local}");
        assert_eq!(
            (parts[0].len(), parts[1].len(), parts[2].len()),
            (4, 2, 2),
            "{local}"
        );
        assert_eq!(
            local,
            date_at_offset(now, local_offset(now)),
            "goes through the machine's own offset"
        );
    }
}
