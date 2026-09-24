//! gateway が自分で数える時間バケット (DR-0030 §3)。
//!
//! 上流が枠を教えてくれなくても、宣言した数で止める。窓は固定窓で、`minute` /
//! `hour` は UTC の整数分 / 整数時、`day` / `month` はバケットごとの基準 tz の
//! 現地日付 / 月で切る。複数のバケットのどれか 1 つでも埋まったら止める。
//!
//! ここにあるのは宣言の形、窓の境界、メモリで数える器まで。数える単位
//! (どの鍵で数えるか) と応答の形は利用側が決める。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use crate::counter::{CounterStore, RequestCount};
use crate::stats::Merged;

use jiff::tz::{Offset, TimeZone};
use jiff::{Timestamp, ToSpan as _};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

/// 窓の長さ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Per {
    Minute,
    Hour,
    Day,
    Month,
}

impl Per {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Month => "month",
        }
    }
}

/// 日 / 月の境界を切る基準の tz。設定では IANA 名か固定 offset (`±HH:MM`)。
#[derive(Clone, PartialEq)]
pub struct Zone {
    written: String,
    tz: TimeZone,
}

impl Zone {
    pub fn utc() -> Self {
        Self {
            written: "UTC".to_owned(),
            tz: TimeZone::UTC,
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        let tz = match parse_offset(raw) {
            Some(offset) => offset.to_time_zone(),
            None => TimeZone::get(raw).map_err(|e| {
                format!(
                    "`{raw}` is neither an IANA time zone name nor an offset like `+09:00`: {e}"
                )
            })?,
        };
        Ok(Self {
            written: raw.to_owned(),
            tz,
        })
    }
}

impl fmt::Debug for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.written)
    }
}

/// `+09:00` / `-05:30` を読む。形が違えば `None` (IANA 名として読む側へ回す)。
fn parse_offset(raw: &str) -> Option<Offset> {
    let (sign, rest) = match raw.as_bytes().first()? {
        b'+' => (1, &raw[1..]),
        b'-' => (-1, &raw[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    if h > 23 || m > 59 {
        return None;
    }
    Offset::from_seconds(sign * (h * 3600 + m * 60)).ok()
}

/// 1 つのバケットの宣言。
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    pub requests: u64,
    pub per: Per,
    /// `day` / `month` の基準。`minute` / `hour` では常に UTC。
    pub tz: Zone,
}

impl Limit {
    /// 知らせや応答で名指す名前。`minute` / `day@Asia/Tokyo`。
    pub fn label(&self) -> String {
        match self.per {
            Per::Minute | Per::Hour => self.per.as_str().to_owned(),
            Per::Day | Per::Month => format!("{}@{}", self.per.as_str(), self.tz.written),
        }
    }

    /// `now_secs` (unix 秒) を含む窓の `[始まり, 終わり)` (unix 秒)。
    ///
    /// 日 / 月は現地の 00:00 から次の 00:00 まで。DST の日は 23 / 25 時間になる
    /// (上限は変えない)。00:00 が存在しない日は、その日の最初の有効な時刻
    /// (jiff の `compatible` の解決規則) に寄せる。
    pub fn window(&self, now_secs: i64) -> (i64, i64) {
        let fixed = |len: i64| {
            let start = now_secs.div_euclid(len) * len;
            (start, start + len)
        };
        match self.per {
            Per::Minute => fixed(60),
            Per::Hour => fixed(3600),
            Per::Day | Per::Month => {
                let zoned = Timestamp::from_second(now_secs)
                    .unwrap_or(Timestamp::UNIX_EPOCH)
                    .to_zoned(self.tz.tz.clone());
                let bounds = || -> Result<(i64, i64), jiff::Error> {
                    let (start, next) = if self.per == Per::Day {
                        let start = zoned.start_of_day()?;
                        let next = zoned.tomorrow()?.start_of_day()?;
                        (start, next)
                    } else {
                        let start = zoned.first_of_month()?.start_of_day()?;
                        let next = zoned
                            .first_of_month()?
                            .checked_add(1.month())?
                            .start_of_day()?;
                        (start, next)
                    };
                    Ok((start.timestamp().as_second(), next.timestamp().as_second()))
                };
                // 暦の範囲外 (西暦 9999 年の先など) では UTC の日で代える。
                bounds().unwrap_or_else(|_| fixed(86_400))
            }
        }
    }
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            requests: u64,
            per: Per,
            #[serde(default)]
            tz: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.requests == 0 {
            return Err(de::Error::custom(
                "limits.requests must be 1 or more (remove the entry to stop limiting)",
            ));
        }
        let tz = match (&raw.tz, raw.per) {
            (Some(_), Per::Minute | Per::Hour) => {
                return Err(de::Error::custom(format!(
                    "limits with per = \"{}\" are cut on whole UTC minutes / hours; remove `tz`",
                    raw.per.as_str()
                )));
            }
            (Some(written), _) => Zone::parse(written).map_err(de::Error::custom)?,
            (None, _) => Zone::utc(),
        };
        Ok(Self {
            requests: raw.requests,
            per: raw.per,
            tz,
        })
    }
}

impl Serialize for Limit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("requests", &self.requests)?;
        map.serialize_entry("per", &self.per)?;
        if matches!(self.per, Per::Day | Per::Month) && self.tz.written != "UTC" {
            map.serialize_entry("tz", &self.tz.written)?;
        }
        map.end()
    }
}

/// 応答に出す残量 (`X-RateLimit-*`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quota {
    pub limit: u64,
    pub remaining: u64,
    /// 窓が終わるまでの秒 (切り上げ)。
    pub reset_secs: u64,
}

/// 埋まっていて通さなかった。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exceeded {
    /// 埋まったバケットのうち、窓の終わりが最も遅いものの名前。
    pub bucket: String,
    /// そのバケットが空くまでの秒 (切り上げ)。早く空くものを返すと、戻ってきて
    /// また断られる。
    pub retry_after_secs: u64,
    pub quota: Quota,
}

/// 通さなかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// 枠が埋まっている (429)。
    Exceeded(Exceeded),
    /// 日 / 月の枠を数えられない (503)。他の書き手の分が読めない、または自分の
    /// 分を書けない。枠が埋まったのではなく、gateway が判定できない状態。
    Unavailable(String),
}

/// 鍵ごと・バケットごとの固定窓のカウンタ。
///
/// `minute` / `hour` はメモリだけで持ち、再起動で消える。`day` / `month` は
/// 再起動と書き手を跨いで数えるので [`CounterStore`] に置き、fail-closed で
/// 扱う: 他の書き手の分が 1 つでも読めなければ通さず、送る前に自分の累計を
/// 書き、書けなければ通さない (DR-0031 §2 (3))。
///
/// 書き手の間で「読む → 判定 → 書く」が重なると、上限を書き手の数 − 1 件まで
/// 超えうる (裁定済み、計画 rate-limit-and-allowlist §7)。
pub struct RateLimiter {
    /// (鍵, バケットの番号) → (窓の始まり, 数)。1 本の判定と書き込みはこの錠の
    /// 中で行うので、同じ書き手の中では重ならない。
    memory: Mutex<HashMap<(String, usize), (i64, u64)>>,
    store: Box<dyn CounterStore<Windows>>,
}

/// 1 つの鍵の日 / 月の窓ごとの数。鍵 1 つ・書き手 1 つにつき 1 ファイルに
/// まとめるので、複数の窓への加算が 1 回の書き込み (rename) で揃って入る。
/// 窓の名前は [`window_key`]。
pub type Windows = std::collections::BTreeMap<String, RequestCount>;

/// 何も置かない器。`day` / `month` を宣言しない使い方のためにある。
struct Nowhere;

impl CounterStore<Windows> for Nowhere {
    fn read_own(&self, _bucket: &str) -> std::io::Result<Windows> {
        Err(std::io::Error::other("no place to keep day / month counts"))
    }
    fn write_own(&self, _bucket: &str, _value: &Windows) -> std::io::Result<()> {
        Err(std::io::Error::other("no place to keep day / month counts"))
    }
    fn read_merged(&self, _bucket: &str) -> Merged<Windows> {
        Merged {
            value: Windows::new(),
            missing: vec!["*".to_owned()],
        }
    }
}

/// 数える途中の 1 バケット。
struct Seen<'a> {
    index: usize,
    limit: &'a Limit,
    used: u64,
    end: i64,
    /// 日 / 月なら置き場の鍵。
    stored: Option<String>,
}

impl RateLimiter {
    /// `minute` / `hour` だけを数える器。
    pub fn in_memory() -> Self {
        Self::new(Box::new(Nowhere))
    }

    /// `day` / `month` を `store` に置く器。
    pub fn new(store: Box<dyn CounterStore<Windows>>) -> Self {
        Self {
            memory: Mutex::new(HashMap::new()),
            store,
        }
    }

    /// 1 本を受けてよいか。よければ全バケットに 1 を足し、最も詰まっている
    /// バケットの残量を返す。どれか 1 つでも埋まっていれば、どこにも足さずに断る。
    pub fn take(
        &self,
        key: &str,
        limits: &[Limit],
        now_secs: i64,
    ) -> Result<Option<Quota>, Refused> {
        if limits.is_empty() {
            return Ok(None);
        }
        let mut memory = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
        // 日 / 月は全部の窓を 1 回で読む (鍵 1 つにつき 1 ファイル)。
        let stored = if limits
            .iter()
            .any(|l| matches!(l.per, Per::Day | Per::Month))
        {
            let merged = self.store.read_merged(key);
            if !merged.missing.is_empty() {
                return Err(Refused::Unavailable(format!(
                    "cannot read the day / month counts of: {}",
                    merged.missing.join(", ")
                )));
            }
            merged.value
        } else {
            Windows::new()
        };
        let mut seen = Vec::with_capacity(limits.len());
        for (index, limit) in limits.iter().enumerate() {
            let (start, end) = limit.window(now_secs);
            match limit.per {
                Per::Minute | Per::Hour => {
                    let slot = memory.entry((key.to_owned(), index)).or_insert((start, 0));
                    // 窓が進んだら数え直す。時計が戻って前の窓を指した時は、数は
                    // 覚えている窓のまま持つ (数え直すと、既に数えた分を忘れて
                    // 通しすぎる)。空くまでの秒は今の時刻の窓で数える (覚えている
                    // 窓の終わりで数えると、戻った分だけ長く待たせる)。
                    if start > slot.0 {
                        *slot = (start, 0);
                    }
                    seen.push(Seen {
                        index,
                        limit,
                        used: slot.1,
                        end,
                        stored: None,
                    });
                }
                Per::Day | Per::Month => {
                    let window = window_key(limit, start);
                    seen.push(Seen {
                        index,
                        limit,
                        used: stored.get(&window).map_or(0, |c| c.0),
                        end,
                        stored: Some(window),
                    });
                }
            }
        }

        let quota_of = |limit: &Limit, used: u64, end: i64| Quota {
            limit: limit.requests,
            remaining: limit.requests.saturating_sub(used),
            reset_secs: (end - now_secs).max(0) as u64,
        };
        if let Some(full) = seen
            .iter()
            .filter(|s| s.used >= s.limit.requests)
            .max_by_key(|s| s.end)
        {
            return Err(Refused::Exceeded(Exceeded {
                bucket: full.limit.label(),
                retry_after_secs: (full.end - now_secs).max(1) as u64,
                quota: quota_of(full.limit, full.used, full.end),
            }));
        }

        // 置き場へ先に書く。書けなければメモリにも足さずに断る (送る前に数える。
        // 送ってから書くと、書き損ねた分だけ枠を超える)。全部の窓を 1 回で書く
        // ので、一部の窓だけ数えた状態は残らない。
        //
        // 落とすのは、終わってから 1 窓分以上たった窓だけ。今の窓・直前の窓・
        // 先の窓は残す。時計が前の窓へ戻った時に、その窓の数が消えて枠が丸ごと
        // 戻るのを防ぐ (分 / 時が数を保持するのと同じ)。
        if seen.iter().any(|s| s.stored.is_some()) {
            let own = self
                .store
                .read_own(key)
                .map_err(|e| Refused::Unavailable(format!("cannot read own count: {e}")))?;
            let mut next: Windows = own
                .into_iter()
                .filter(|(window, _)| still_relevant(window, now_secs))
                .collect();
            for window in seen.iter().filter_map(|s| s.stored.as_ref()) {
                next.entry(window.clone()).or_default().0 += 1;
            }
            self.store
                .write_own(key, &next)
                .map_err(|e| Refused::Unavailable(format!("cannot write own count: {e}")))?;
        }
        for s in &seen {
            if s.stored.is_none()
                && let Some(slot) = memory.get_mut(&(key.to_owned(), s.index))
            {
                slot.1 += 1;
            }
        }
        // 残量の比率が最も小さいもの (最も詰まっているもの) を出す。
        let tightest = seen
            .iter()
            .map(|s| quota_of(s.limit, s.used + 1, s.end))
            .min_by(|a, b| {
                let ra = a.remaining as f64 / a.limit as f64;
                let rb = b.remaining as f64 / b.limit as f64;
                ra.total_cmp(&rb)
            });
        Ok(tightest)
    }
}

/// 日 / 月の窓の名前。`<per>@<tz>@<窓の始まりの unix 秒>`。
///
/// 窓の始まりを名前に入れるので、窓が進めば別の名前になり数え直しになる。
/// tz は書かれたまま入れる (同じ始まりを持つ別の宣言と混ざらない)。ファイル
/// 名ではなくファイルの中の鍵なので、符号化は要らない。
fn window_key(limit: &Limit, start: i64) -> String {
    format!("{}@{}@{start}", limit.per.as_str(), limit.tz.written)
}

/// 置き場に残しておく窓か。終わりが `now − 1 窓分` より前の窓だけを落とす。
///
/// 窓の長さは名前の `per` から見積もる (日は DST の 25 時間、月は 31 日)。長めに
/// 見積もるのは、残しすぎは害が無く、消しすぎると数を失うため。名前の読めない
/// ものは落とす。
fn still_relevant(window: &str, now_secs: i64) -> bool {
    let mut parts = window.splitn(2, '@');
    let per = parts.next().unwrap_or("");
    let Some(start) = window
        .rsplit('@')
        .next()
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return false;
    };
    let len = match per {
        "day" => 25 * 3600,
        "month" => 31 * 86_400,
        _ => return false,
    };
    start.saturating_add(2 * len) >= now_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(text: &str) -> Limit {
        #[derive(Deserialize)]
        struct Wrap {
            l: Limit,
        }
        toml::from_str::<Wrap>(&format!("l = {text}")).unwrap().l
    }

    fn exceeded(result: Result<Option<Quota>, Refused>) -> Exceeded {
        match result {
            Err(Refused::Exceeded(e)) => e,
            other => panic!("expected a full bucket, got {other:?}"),
        }
    }

    fn at(rfc3339: &str) -> i64 {
        rfc3339.parse::<Timestamp>().unwrap().as_second()
    }

    #[test]
    fn minutes_and_hours_are_whole_utc_units() {
        let m = limit(r#"{ requests = 1, per = "minute" }"#);
        assert_eq!(
            m.window(at("2026-09-24T10:15:42Z")),
            (at("2026-09-24T10:15:00Z"), at("2026-09-24T10:16:00Z"))
        );
        let h = limit(r#"{ requests = 1, per = "hour" }"#);
        assert_eq!(
            h.window(at("2026-09-24T10:15:42Z")),
            (at("2026-09-24T10:00:00Z"), at("2026-09-24T11:00:00Z"))
        );
    }

    #[test]
    fn a_day_starts_at_local_midnight() {
        let tokyo = limit(r#"{ requests = 1, per = "day", tz = "Asia/Tokyo" }"#);
        // 東京の 0 時 = 前日 15:00 UTC。
        assert_eq!(
            tokyo.window(at("2026-09-24T14:59:59Z")),
            (at("2026-09-23T15:00:00Z"), at("2026-09-24T15:00:00Z"))
        );
        assert_eq!(
            tokyo.window(at("2026-09-24T15:00:00Z")).0,
            at("2026-09-24T15:00:00Z")
        );

        let fixed = limit(r#"{ requests = 1, per = "day", tz = "-05:30" }"#);
        assert_eq!(
            fixed.window(at("2026-09-24T03:00:00Z")),
            (at("2026-09-23T05:30:00Z"), at("2026-09-24T05:30:00Z"))
        );
        assert_eq!(fixed.label(), "day@-05:30");

        let utc = limit(r#"{ requests = 1, per = "day" }"#);
        assert_eq!(
            utc.window(at("2026-09-24T23:59:59Z")).1,
            at("2026-09-25T00:00:00Z")
        );
    }

    /// DST の切替日は窓が 23 / 25 時間になる。
    #[test]
    fn a_dst_day_is_shorter_or_longer() {
        let la = limit(r#"{ requests = 1, per = "day", tz = "America/Los_Angeles" }"#);
        let (s, e) = la.window(at("2026-03-08T12:00:00Z"));
        assert_eq!(e - s, 23 * 3600, "spring forward");
        let (s, e) = la.window(at("2026-11-01T12:00:00Z"));
        assert_eq!(e - s, 25 * 3600, "fall back");
    }

    #[test]
    fn a_month_follows_the_calendar() {
        let utc = limit(r#"{ requests = 1, per = "month" }"#);
        assert_eq!(
            utc.window(at("2028-02-29T12:00:00Z")),
            (at("2028-02-01T00:00:00Z"), at("2028-03-01T00:00:00Z")),
            "a leap February"
        );
        assert_eq!(
            utc.window(at("2026-12-31T23:59:59Z")).1,
            at("2027-01-01T00:00:00Z")
        );
        let tokyo = limit(r#"{ requests = 1, per = "month", tz = "+09:00" }"#);
        assert_eq!(
            tokyo.window(at("2026-09-30T15:00:00Z")).0,
            at("2026-09-30T15:00:00Z")
        );
    }

    #[test]
    fn bad_declarations_say_why() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Wrap {
            l: Limit,
        }
        for bad in [
            r#"{ requests = 1, per = "minute", tz = "UTC" }"#,
            r#"{ requests = 0, per = "day" }"#,
            r#"{ requests = 1, per = "week" }"#,
            r#"{ requests = 1, per = "day", tz = "Mars/Olympus" }"#,
        ] {
            assert!(
                toml::from_str::<Wrap>(&format!("l = {bad}")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_bucket_fills_and_reopens_on_the_next_window() {
        let buckets = RateLimiter::in_memory();
        let limits = [limit(r#"{ requests = 2, per = "minute" }"#)];
        let t = at("2026-09-24T10:15:10Z");
        assert_eq!(buckets.take("k", &limits, t).unwrap().unwrap().remaining, 1);
        assert_eq!(buckets.take("k", &limits, t).unwrap().unwrap().remaining, 0);
        let refused = exceeded(buckets.take("k", &limits, t));
        assert_eq!(refused.retry_after_secs, 50);
        assert_eq!(refused.bucket, "minute");
        // 別の鍵は別に数える。
        assert!(buckets.take("other", &limits, t).is_ok());
        // 窓が進めば空く。時計が戻っても前の窓で数え直さない。
        assert!(buckets.take("k", &limits, t + 50).is_ok());
        assert!(buckets.take("k", &limits, t - 60).is_ok());
        assert!(
            buckets.take("k", &limits, t - 60).is_err(),
            "counted in the current window"
        );
    }

    /// どれか 1 つでも埋まれば止め、埋まっていない側にも数を足さない。
    #[test]
    fn every_bucket_must_have_room() {
        let buckets = RateLimiter::in_memory();
        let limits = [
            limit(r#"{ requests = 5, per = "minute" }"#),
            limit(r#"{ requests = 2, per = "hour" }"#),
        ];
        let t = at("2026-09-24T10:15:10Z");
        let q = buckets.take("k", &limits, t).unwrap().unwrap();
        assert_eq!(
            (q.limit, q.remaining),
            (2, 1),
            "the tightest one is reported"
        );
        buckets.take("k", &limits, t).unwrap();
        let refused = exceeded(buckets.take("k", &limits, t + 120));
        assert_eq!(refused.bucket, "hour");
        assert_eq!(
            refused.retry_after_secs,
            (at("2026-09-24T11:00:00Z") - (t + 120)) as u64
        );
        // 次の分の窓でも、時の窓が埋まっている間は通らない。
        assert!(buckets.take("k", &limits, t + 180).is_err());
    }

    fn stored(dir: &std::path::Path, writer: &str) -> RateLimiter {
        RateLimiter::new(Box::new(crate::counter::FileCounters::new(dir, writer)))
    }

    /// 日次 5,000 本で 5,001 本目は断る。器を作り直しても (再起動) 累計は残る。
    #[test]
    fn a_day_counts_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let limits = [limit(
            r#"{ requests = 5000, per = "day", tz = "Asia/Tokyo" }"#,
        )];
        let t = at("2026-09-24T03:00:00Z");
        // 4,998 本を数えた書き手の置き場を用意する (1 本ずつ書くと遅いので、累計を直に置く)。
        let window = window_key(&limits[0], limits[0].window(t).0);
        let files = crate::counter::FileCounters::new(dir.path(), "a");
        files
            .write_own("k", &Windows::from([(window, RequestCount(4998))]))
            .unwrap();
        {
            let first = stored(dir.path(), "a");
            first.take("k", &limits, t).unwrap();
        }
        // 作り直しても (再起動)、4,999 本目までを覚えている。
        let again = stored(dir.path(), "a");
        let q = again.take("k", &limits, t).unwrap().unwrap();
        assert_eq!(q.remaining, 0, "the 5,000th fits");
        let refused = exceeded(again.take("k", &limits, t));
        assert_eq!(refused.bucket, "day@Asia/Tokyo");
        assert_eq!(
            refused.retry_after_secs,
            (at("2026-09-24T15:00:00Z") - t) as u64
        );
    }

    /// 2 つの書き手の分は合わせて数える。
    #[test]
    fn two_writers_share_one_day() {
        let dir = tempfile::tempdir().unwrap();
        let limits = [limit(r#"{ requests = 3, per = "day" }"#)];
        let t = at("2026-09-24T03:00:00Z");
        let (a, b) = (stored(dir.path(), "a"), stored(dir.path(), "b"));
        a.take("k", &limits, t).unwrap();
        b.take("k", &limits, t).unwrap();
        let q = a.take("k", &limits, t).unwrap().unwrap();
        assert_eq!(q.remaining, 0);
        exceeded(b.take("k", &limits, t));
    }

    /// 他の書き手の分が読めなければ、枠に余裕があっても通さない。
    #[test]
    fn an_unreadable_writer_stops_the_day() {
        let dir = tempfile::tempdir().unwrap();
        let limits = [limit(r#"{ requests = 100, per = "day" }"#)];
        let t = at("2026-09-24T03:00:00Z");
        let a = stored(dir.path(), "a");
        a.take("k", &limits, t).unwrap();
        std::fs::write(dir.path().join("k.b.json"), "{ broken").unwrap();
        assert!(matches!(a.take("k", &limits, t), Err(Refused::Unavailable(m)) if m.contains('b')));
    }

    /// 自分の分を書けなければ通さず、分のバケットにも数えない。
    #[test]
    fn a_failed_write_stops_the_request() {
        let dir = tempfile::tempdir().unwrap();
        // 置き場のはずの場所をファイルで塞ぐ。
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let limiter = stored(&blocked, "a");
        let limits = [
            limit(r#"{ requests = 1, per = "minute" }"#),
            limit(r#"{ requests = 100, per = "day" }"#),
        ];
        let t = at("2026-09-24T03:00:00Z");
        assert!(matches!(
            limiter.take("k", &limits, t),
            Err(Refused::Unavailable(_))
        ));
        let only_minute = [limit(r#"{ requests = 1, per = "minute" }"#)];
        assert!(
            limiter.take("k", &only_minute, t).is_ok(),
            "the minute was not spent"
        );
    }

    /// 月の窓が進めば (tz 付きの境界で) 空く。
    #[test]
    fn a_month_reopens_at_the_local_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let limits = [limit(r#"{ requests = 1, per = "month", tz = "+09:00" }"#)];
        let a = stored(dir.path(), "a");
        a.take("k", &limits, at("2026-09-30T14:59:59Z")).unwrap();
        exceeded(a.take("k", &limits, at("2026-09-30T14:59:59Z")));
        assert!(
            a.take("k", &limits, at("2026-09-30T15:00:00Z")).is_ok(),
            "October in Tokyo"
        );
    }

    /// 置き場の無い器では、日 / 月は数えられないので通さない。
    #[test]
    fn a_memory_only_limiter_refuses_days() {
        let limits = [limit(r#"{ requests = 1, per = "day" }"#)];
        assert!(matches!(
            RateLimiter::in_memory().take("k", &limits, 0),
            Err(Refused::Unavailable(_))
        ));
    }

    /// 日と月の 2 つの窓は 1 回で書く。書けなければどちらも数えない。
    #[test]
    fn day_and_month_are_written_together() {
        let dir = tempfile::tempdir().unwrap();
        let limits = [
            limit(r#"{ requests = 10, per = "day" }"#),
            limit(r#"{ requests = 10, per = "month" }"#),
        ];
        let t = at("2026-09-24T03:00:00Z");
        let a = stored(dir.path(), "a");
        a.take("k", &limits, t).unwrap();
        let own: Windows = crate::counter::FileCounters::new(dir.path(), "a")
            .read_own("k")
            .unwrap();
        assert_eq!(own.len(), 2);
        assert!(own.values().all(|c| c.0 == 1));
    }

    fn own_windows(dir: &std::path::Path) -> Windows {
        crate::counter::FileCounters::new(dir, "a")
            .read_own("k")
            .unwrap()
    }

    /// 2 窓以上前の窓は、次に書くときにファイルから落ちる。
    #[test]
    fn a_long_past_window_does_not_pile_up() {
        let dir = tempfile::tempdir().unwrap();
        let limits = [limit(r#"{ requests = 10, per = "day" }"#)];
        let a = stored(dir.path(), "a");
        a.take("k", &limits, at("2026-09-22T03:00:00Z")).unwrap();
        a.take("k", &limits, at("2026-09-25T03:00:00Z")).unwrap();
        let own = own_windows(dir.path());
        assert_eq!(own.len(), 1, "{own:?}");
        assert!(
            own.keys()
                .all(|k| k.ends_with(&at("2026-09-25T00:00:00Z").to_string()))
        );
    }

    /// 翌日に数えた後で前日へ戻っても、前日の数は残っていて枠は戻らない。
    #[test]
    fn a_rewind_to_the_previous_day_keeps_its_count() {
        let dir = tempfile::tempdir().unwrap();
        let limits = [limit(r#"{ requests = 1, per = "day" }"#)];
        let a = stored(dir.path(), "a");
        let yesterday = at("2026-09-24T23:00:00Z");
        a.take("k", &limits, yesterday).unwrap();
        a.take("k", &limits, at("2026-09-25T01:00:00Z")).unwrap();
        exceeded(a.take("k", &limits, yesterday));
    }

    /// 先の窓で数えた後に戻って、また進めても、先の窓の数は消えない。
    #[test]
    fn a_future_window_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let limits = [limit(r#"{ requests = 1, per = "day" }"#)];
        let a = stored(dir.path(), "a");
        let tomorrow = at("2026-09-25T01:00:00Z");
        a.take("k", &limits, tomorrow).unwrap();
        a.take("k", &limits, at("2026-09-24T23:00:00Z")).unwrap();
        exceeded(a.take("k", &limits, tomorrow));
    }

    /// tz だけが違う宣言は別の窓として数える。
    #[test]
    fn zones_that_look_alike_are_counted_apart() {
        let plus = limit(r#"{ requests = 1, per = "day", tz = "Etc/GMT+1" }"#);
        let minus = limit(r#"{ requests = 1, per = "day", tz = "Etc/GMT-1" }"#);
        let t = at("2026-09-24T12:00:00Z");
        assert_ne!(
            window_key(&plus, plus.window(t).0),
            window_key(&minus, minus.window(t).0)
        );
    }

    /// 時計が戻っても、空くまでの秒は今の時刻の窓で数える。
    #[test]
    fn a_rewound_clock_does_not_inflate_retry_after() {
        let buckets = RateLimiter::in_memory();
        let limits = [limit(r#"{ requests = 1, per = "hour" }"#)];
        let t = at("2026-09-24T10:30:00Z");
        buckets.take("k", &limits, t).unwrap();
        let rewound = at("2026-09-24T09:59:00Z");
        let refused = exceeded(buckets.take("k", &limits, rewound));
        assert_eq!(
            refused.retry_after_secs, 60,
            "the hour of 09:59 ends in a minute"
        );
    }
}
