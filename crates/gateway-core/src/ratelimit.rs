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

/// 鍵ごと・バケットごとの固定窓のカウンタ。メモリだけで持ち、再起動で消える。
#[derive(Default)]
pub struct MemoryBuckets {
    /// (鍵, バケットの番号) → (窓の始まり, 数)。
    counts: Mutex<HashMap<(String, usize), (i64, u64)>>,
}

impl MemoryBuckets {
    pub fn new() -> Self {
        Self::default()
    }

    /// 1 本を受けてよいか。よければ全バケットに 1 を足し、最も詰まっている
    /// バケットの残量を返す。どれか 1 つでも埋まっていれば、どこにも足さずに断る。
    pub fn take(
        &self,
        key: &str,
        limits: &[Limit],
        now_secs: i64,
    ) -> Result<Option<Quota>, Exceeded> {
        if limits.is_empty() {
            return Ok(None);
        }
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let mut seen = Vec::with_capacity(limits.len());
        for (i, limit) in limits.iter().enumerate() {
            let (start, end) = limit.window(now_secs);
            let slot = counts.entry((key.to_owned(), i)).or_insert((start, 0));
            // 窓が進んだら数え直す。時計が戻って前の窓を指した時は、今の窓の
            // まま数える (数え直すと、既に数えた分を忘れて通しすぎる)。
            if start > slot.0 {
                *slot = (start, 0);
            }
            let end = end.max(slot.0 + 1);
            seen.push((i, limit, slot.1, end));
        }

        let quota_of = |limit: &Limit, used: u64, end: i64| Quota {
            limit: limit.requests,
            remaining: limit.requests.saturating_sub(used),
            reset_secs: (end - now_secs).max(0) as u64,
        };
        let full: Vec<_> = seen
            .iter()
            .filter(|(_, limit, used, _)| *used >= limit.requests)
            .collect();
        if let Some((_, limit, used, end)) = full.iter().max_by_key(|(_, _, _, end)| *end) {
            return Err(Exceeded {
                bucket: limit.label(),
                retry_after_secs: (*end - now_secs).max(1) as u64,
                quota: quota_of(limit, *used, *end),
            });
        }

        for (i, _, _, _) in &seen {
            if let Some(slot) = counts.get_mut(&(key.to_owned(), *i)) {
                slot.1 += 1;
            }
        }
        // 残量の比率が最も小さいもの (最も詰まっているもの) を出す。
        let tightest = seen
            .iter()
            .map(|(_, limit, used, end)| quota_of(limit, used + 1, *end))
            .min_by(|a, b| {
                let ra = a.remaining as f64 / a.limit as f64;
                let rb = b.remaining as f64 / b.limit as f64;
                ra.total_cmp(&rb)
            });
        Ok(tightest)
    }
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
        let buckets = MemoryBuckets::new();
        let limits = [limit(r#"{ requests = 2, per = "minute" }"#)];
        let t = at("2026-09-24T10:15:10Z");
        assert_eq!(buckets.take("k", &limits, t).unwrap().unwrap().remaining, 1);
        assert_eq!(buckets.take("k", &limits, t).unwrap().unwrap().remaining, 0);
        let refused = buckets.take("k", &limits, t).unwrap_err();
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
        let buckets = MemoryBuckets::new();
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
        let refused = buckets.take("k", &limits, t + 120).unwrap_err();
        assert_eq!(refused.bucket, "hour");
        assert_eq!(
            refused.retry_after_secs,
            (at("2026-09-24T11:00:00Z") - (t + 120)) as u64
        );
        // 次の分の窓でも、時の窓が埋まっている間は通らない。
        assert!(buckets.take("k", &limits, t + 180).is_err());
    }
}
