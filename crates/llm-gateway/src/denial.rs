//! 断られた経路を、しばらく試さないでおく (DR-0009)。
//!
//! 印が無いと、上限に当たった認証情報が先頭にいるだけで**すべてのリクエストが
//! 毎回そこに当たって 429 を貰い、次へ回る**。窓が開くまで、1 本あたり 1 往復
//! ぶんの遅れが乗り続ける。
//!
//! 断られ方で空ける長さが変わる:
//!
//! - **上限** ([`Reason::Limited`]) — 429 と一緒に、どの窓が塞がっていて
//!   いつ開くかが返る。開く時刻が分かっているのだから、そこまでは実際の
//!   リクエストで一度も選ばない
//! - **混雑** ([`Reason::Busy`]) — 529 や、上限のヘッダを伴わない 429。
//!   いつ空くかは誰も知らないので、短く退避して様子を見るだけ
//!
//! 印はメモリだけに持つ。落として失うのは再起動直後の 1 回の空振りで、その
//! 1 回が印を付け直す。ディスクに置くと、もう空いている経路を古い印で
//! 締め出す方向の間違いが増える。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::router::Route;
use crate::usage::{Snapshot, Window};

/// いつ空くかの手掛かりが何も無いときに空ける間隔 (秒)。
const DEFAULT_BACKOFF: i64 = 60;

/// 締め出し中の経路に、裏で様子を聞きに行く間隔 (秒)。
///
/// 上限のリセット時刻は「遅くともここまでには開く」であって、それより早く
/// 開くこともある。開いたことに気づく手段が実リクエストしか無いと、印を
/// 付けている間は永久に気づけない。
pub const PROBE_INTERVAL: i64 = 60 * 60;

/// 断られた理由。空ける長さと、様子を聞きに行くかが変わる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 上限に当たった。いつ開くかが分かっている。
    Limited,
    /// 一時的に混んでいる。いつ空くかは分からない。
    Busy,
}

/// この経路は、いつまで断られているか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denial {
    /// この時刻 (Unix 秒) までは候補にしない。
    pub until: i64,
    pub reason: Reason,
}

/// この応答は経路を締め出すか。するならいつまで、どの理由で。
///
/// - **429 + 塞がっている窓** → その窓が開く時刻まで ([`Reason::Limited`])。
///   窓が複数塞がっているなら、最も近い開く時刻まで (5 時間の窓が埋まって
///   いても 7 日の窓に余裕があるなら、5 時間の方で開く)
/// - **429 で窓が読めない / 529** → `retry-after`、無ければ
///   [`DEFAULT_BACKOFF`] だけ ([`Reason::Busy`])
/// - それ以外の状態 → 締め出さない。401 / 403 は待っても直らないので、
///   時間で空ける印を付ける意味がない
pub fn denial_of(status: u16, headers: &[(String, String)], now: i64) -> Option<Denial> {
    if !matches!(status, 429 | 529) {
        return None;
    }
    if status == 429
        && let Some(reset) = nearest_reset(headers, now)
    {
        return Some(Denial {
            until: reset,
            reason: Reason::Limited,
        });
    }
    let after = retry_after(headers).unwrap_or(DEFAULT_BACKOFF);
    Some(Denial {
        until: now + after.max(0),
        reason: Reason::Busy,
    })
}

/// 塞がっている窓のうち、最も近い開く時刻。
fn nearest_reset(headers: &[(String, String)], now: i64) -> Option<i64> {
    let snapshot = Snapshot::from_headers(headers, now)?;
    [snapshot.five_hour, snapshot.seven_day]
        .into_iter()
        .flatten()
        .filter(is_rejected)
        .filter_map(|w| w.reset)
        // 過ぎている時刻は手掛かりにならない。次の手掛かりへ落とす。
        .filter(|reset| *reset > now)
        .min()
}

/// この窓は塞がっているか。
///
/// 通っている側の語 (`allowed`, `allowed_warning`) だけを数え、それ以外を
/// 塞がっている扱いにする。語彙は公式に記載が無く増えうる (DR-0007) ので、
/// 知らない語で締め出しを見送ると、印が付かないまま毎回 429 を貰い続ける
/// 元の状態に戻る。
fn is_rejected(window: &Window) -> bool {
    window
        .status
        .as_deref()
        .is_some_and(|s| !s.trim().to_ascii_lowercase().starts_with("allowed"))
}

/// `retry-after` の秒数。HTTP-date 形式なら読まない (既定へ落とす)。
fn retry_after(headers: &[(String, String)]) -> Option<i64> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// 今このリクエストで試してよい経路。
#[derive(Debug)]
pub enum Candidates {
    /// 断られていない経路。設定の優先順のまま。
    Ready(Vec<Arc<Route>>),
    /// どれも断られている。いつ空くか (最も近い時刻) だけ返す。
    AllDenied { until: i64 },
}

/// 経路ごとの締め出しの印。
///
/// 鍵は経路の名前 (= 設定に書いた credential の名前)。認証情報を持たない
/// 経路 (relay) にも 529 は返るので、`CredentialId` ではなく経路の名前で持つ。
#[derive(Default)]
pub struct Denials {
    marks: Mutex<HashMap<String, Mark>>,
}

struct Mark {
    denial: Denial,
    /// 最後にこの経路の状態を実際に聞いた時刻。次に様子を聞く時期をここから測る。
    seen_at: i64,
    /// いま裏で様子を聞いている最中。同じ相手に一斉に聞きに行かないための札。
    probing: bool,
}

impl Denials {
    pub fn new() -> Self {
        Self::default()
    }

    fn marks(&self) -> MutexGuard<'_, HashMap<String, Mark>> {
        self.marks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 断られたことを控える。
    ///
    /// 上限 ([`Reason::Limited`]) は上限のヘッダが正本なので、そのまま置く。
    /// 混雑 ([`Reason::Busy`]) は当て推量なので、**先に控えた期限を縮めない** —
    /// 60 秒の退避で、読めていた窓の開く時刻を上書きしてはいけない。
    pub fn deny(&self, route: &str, denial: Denial, now: i64) {
        let mut marks = self.marks();
        let kept = marks.get(route);
        let probing = kept.is_some_and(|m| m.probing);
        let denial = match kept {
            Some(m) if denial.reason == Reason::Busy && m.denial.until >= denial.until => m.denial,
            _ => denial,
        };
        marks.insert(
            route.to_owned(),
            Mark {
                denial,
                seen_at: now,
                probing,
            },
        );
    }

    /// 印を消す。通った (2xx) 経路に対して呼ぶ。
    pub fn allow(&self, route: &str) {
        self.marks().remove(route);
    }

    /// この経路の締め出し。空いていれば `None`。
    pub fn get(&self, route: &str, now: i64) -> Option<Denial> {
        self.marks()
            .get(route)
            .map(|m| m.denial)
            .filter(|d| d.until > now)
    }

    /// 今このリクエストで試せる経路を選ぶ。
    ///
    /// 断られている経路は**外す**。開く時刻を知っていながら実リクエストを
    /// 当てるのは、分かっている壁にわざわざぶつかりに行くのと同じ。
    pub fn candidates(&self, routes: &[Arc<Route>], now: i64) -> Candidates {
        let marks = self.marks();
        let denial = |route: &Arc<Route>| {
            marks
                .get(route.name())
                .map(|m| m.denial)
                .filter(|d| d.until > now)
        };

        let ready: Vec<Arc<Route>> = routes
            .iter()
            .filter(|r| denial(r).is_none())
            .cloned()
            .collect();
        if !ready.is_empty() {
            return Candidates::Ready(ready);
        }
        Candidates::AllDenied {
            // どれも塞がっているなら、最初に開くのがいつかがクライアントの知りたいこと。
            until: routes
                .iter()
                .filter_map(|r| denial(r).map(|d| d.until))
                .min()
                .unwrap_or(now),
        }
    }

    /// 様子を聞きに行く役を引き受ける。引き受けられたら札を返す。
    ///
    /// 聞きに行くのは上限で締め出している経路だけ。混雑は開く時刻を持たない
    /// ので短い退避で足り、定期的に聞いても実費が増えるだけになる。
    pub fn claim_probe(self: &Arc<Self>, route: &str, now: i64) -> Option<Probing> {
        let mut marks = self.marks();
        let mark = marks.get_mut(route)?;
        if mark.denial.reason != Reason::Limited
            || mark.probing
            || now - mark.seen_at < PROBE_INTERVAL
        {
            return None;
        }
        mark.probing = true;
        mark.seen_at = now;
        Some(Probing {
            denials: Arc::clone(self),
            route: route.to_owned(),
        })
    }
}

/// 様子を聞いている間の札。持つ者を 1 人に保つ。
///
/// 札を外すのを [`Drop`] に寄せるのは、聞きに行った仕事が途中で落ちても
/// 同じ道を通すため。走り切った経路だけで外すと、落ちたときに札が残り、
/// その経路には二度と聞きに行けなくなる ([`crate::credential`] の更新と同じ形)。
pub struct Probing {
    denials: Arc<Denials>,
    route: String,
}

impl Probing {
    /// 聞きに行く相手。
    pub fn route(&self) -> &str {
        &self.route
    }
}

impl Drop for Probing {
    fn drop(&mut self) {
        if let Some(mark) = self.denials.marks().get_mut(&self.route) {
            mark.probing = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// 1 つの窓を表すヘッダ。
    fn window(prefix: &str, status: &str, reset: i64) -> Vec<(String, String)> {
        headers(&[
            (
                &format!("anthropic-ratelimit-unified-{prefix}-status"),
                status,
            ),
            (
                &format!("anthropic-ratelimit-unified-{prefix}-reset"),
                &reset.to_string(),
            ),
        ])
    }

    fn limited(until: i64) -> Denial {
        Denial {
            until,
            reason: Reason::Limited,
        }
    }

    fn busy(until: i64) -> Denial {
        Denial {
            until,
            reason: Reason::Busy,
        }
    }

    /// 塞がっている窓があれば、そこが開く時刻まで。
    #[test]
    fn a_rejected_window_sets_the_deadline() {
        let mut h = window("5h", "allowed", NOW + 100);
        h.extend(window("7d", "rejected", NOW + 5000));
        assert_eq!(denial_of(429, &h, NOW), Some(limited(NOW + 5000)));
    }

    /// 塞がっている窓が複数あるなら、先に開く方まで。
    #[test]
    fn the_nearest_rejected_window_wins() {
        let mut h = window("5h", "rejected", NOW + 100);
        h.extend(window("7d", "rejected", NOW + 5000));
        assert_eq!(denial_of(429, &h, NOW), Some(limited(NOW + 100)));
    }

    /// 遠い開く時刻でも縮めない。開く時刻を知っているならそこまで待つ。
    #[test]
    fn a_distant_reset_is_not_shortened() {
        let h = window("7d", "rejected", NOW + 400 * 24 * 3600);
        assert_eq!(
            denial_of(429, &h, NOW),
            Some(limited(NOW + 400 * 24 * 3600))
        );
    }

    /// 警告つきでも通ってはいる。塞がっている扱いにしない。
    #[test]
    fn a_warning_window_is_still_open() {
        let mut h = window("5h", "allowed_warning", NOW + 100);
        h.extend(headers(&[("retry-after", "30")]));
        assert_eq!(
            denial_of(429, &h, NOW),
            Some(busy(NOW + 30)),
            "窓は塞がっていないので、混雑として短く退避する"
        );
    }

    /// 窓が読めない 429 は、混雑と同じ短い退避。
    ///
    /// 実測 (2026-07-31): opus-5 / sonnet-5 は上限のヘッダを 1 つも載せない
    /// 素の 429 を返すことがある。利用率が低くても起きる。
    #[test]
    fn a_bare_rate_limit_is_treated_as_congestion() {
        assert_eq!(denial_of(429, &[], NOW), Some(busy(NOW + DEFAULT_BACKOFF)));
        assert_eq!(
            denial_of(429, &headers(&[("retry-after", "30")]), NOW),
            Some(busy(NOW + 30))
        );
    }

    /// 529 は開く時刻を持たない。窓のヘッダが載っていても短い退避のまま。
    #[test]
    fn an_overloaded_upstream_only_steps_aside() {
        let h = window("7d", "rejected", NOW + 5000);
        assert_eq!(denial_of(529, &h, NOW), Some(busy(NOW + DEFAULT_BACKOFF)));
    }

    /// 過ぎた開く時刻は使わない。
    #[test]
    fn a_past_reset_is_ignored() {
        let mut h = window("7d", "rejected", NOW - 10);
        h.extend(headers(&[("retry-after", "30")]));
        assert_eq!(denial_of(429, &h, NOW), Some(busy(NOW + 30)));
    }

    /// 日付形式の `retry-after` は読まない。
    #[test]
    fn an_http_date_retry_after_falls_back_to_the_default() {
        let h = headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(denial_of(529, &h, NOW), Some(busy(NOW + DEFAULT_BACKOFF)));
    }

    /// 待っても直らない断りは締め出さない。
    #[test]
    fn an_auth_failure_is_not_a_cooldown() {
        for status in [200, 400, 401, 403, 500] {
            assert_eq!(denial_of(status, &[], NOW), None, "{status}");
        }
    }

    #[test]
    fn a_mark_expires() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 10), NOW);
        assert_eq!(d.get("a", NOW), Some(limited(NOW + 10)));
        assert_eq!(d.get("a", NOW + 10), None, "期限は含まない");
    }

    /// 短い退避で、読めていた開く時刻を潰さない。
    #[test]
    fn congestion_does_not_shorten_a_known_deadline() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 5000), NOW);
        d.deny("a", busy(NOW + 60), NOW);
        assert_eq!(d.get("a", NOW), Some(limited(NOW + 5000)));
    }

    /// 上限のヘッダは正本。前より近い時刻でも、そのまま置く。
    #[test]
    fn a_fresh_limit_replaces_the_deadline() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 5000), NOW);
        d.deny("a", limited(NOW + 100), NOW);
        assert_eq!(d.get("a", NOW), Some(limited(NOW + 100)));
    }

    #[test]
    fn success_clears_the_mark() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 5000), NOW);
        d.allow("a");
        assert_eq!(d.get("a", NOW), None);
    }

    /// 様子を聞く役は 1 人だけ。札は必ず外れる。
    #[test]
    fn only_one_prober_at_a_time() {
        let d = Arc::new(Denials::new());
        d.deny("a", limited(NOW + 100_000), NOW);

        let later = NOW + PROBE_INTERVAL;
        let held = d
            .claim_probe("a", later)
            .expect("間隔が空いていれば引き受ける");
        assert_eq!(held.route(), "a");
        assert!(
            d.claim_probe("a", later).is_none(),
            "聞いている最中は誰も割り込まない"
        );
        drop(held);
        assert!(
            d.claim_probe("a", later + PROBE_INTERVAL).is_some(),
            "札を外したら、次の周期でまた引き受けられる"
        );
    }

    /// 間隔が空くまでは聞きに行かない。混雑にはそもそも聞きに行かない。
    #[test]
    fn probing_waits_for_the_interval_and_skips_congestion() {
        let d = Arc::new(Denials::new());
        d.deny("a", limited(NOW + 100_000), NOW);
        assert!(d.claim_probe("a", NOW + PROBE_INTERVAL - 1).is_none());
        assert!(d.claim_probe("a", NOW + PROBE_INTERVAL).is_some());

        d.deny("b", busy(NOW + 100_000), NOW);
        assert!(
            d.claim_probe("b", NOW + PROBE_INTERVAL).is_none(),
            "いつ空くか分からない相手に定期的に聞いても、実費が増えるだけ"
        );
    }

    #[test]
    fn an_unmarked_route_is_not_probed() {
        let d = Arc::new(Denials::new());
        assert!(d.claim_probe("a", NOW).is_none());
    }
}
