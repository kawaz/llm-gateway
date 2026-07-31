//! 断られた経路を、しばらく試さないでおく (DR-0009)。
//!
//! 429 / 529 を返した経路には「いつまで断られているか」の印を付け、その間は
//! 経路の候補から外す。印が無いと、上限に当たった認証情報が先頭にいるだけで
//! **すべてのリクエストが毎回そこに当たって 429 を貰い、次へ回る**。上限の
//! リセットまで、1 本あたり 1 往復ぶんの遅れが乗り続ける。
//!
//! 印はメモリだけに持つ。落としても失うのは「再起動直後の 1 回の空振り」で、
//! その 1 回が印を付け直す。ディスクに置くと、実際にはもう空いている経路を
//! 古い印で締め出す方向の間違いが増える。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::router::Route;
use crate::usage::{Snapshot, Window};

/// 期限の手掛かりが何も無いときに空ける間隔 (秒)。
///
/// 529 (混雑) は上限のヘッダを伴わないことが多く、たいていここに落ちる。
const DEFAULT_COOLDOWN: i64 = 60;

/// どんな手掛かりが来ても、これ以上は締め出さない (秒)。
///
/// 時計のずれや、遠すぎるリセット時刻で経路を恒久的に失わないため。
const MAX_COOLDOWN: i64 = 24 * 60 * 60;

/// 経路ごとの「いつまで断られているか」。
///
/// 鍵は経路の名前 (= 設定に書いた credential の名前)。認証情報を持たない
/// 経路 (relay) にも 529 は返るので、`CredentialId` ではなく経路の名前で持つ。
#[derive(Default)]
pub struct Denials {
    until: RwLock<HashMap<String, i64>>,
}

impl Denials {
    pub fn new() -> Self {
        Self::default()
    }

    /// この経路を `until` (Unix 秒) まで候補から外す。
    ///
    /// すでに印があるなら遠い方を残す。同じ窓に何度も当たったとき、後から来た
    /// 短い既定値で先に読めていたリセット時刻を上書きしない。
    pub async fn deny(&self, route: &str, until: i64) {
        let mut marks = self.until.write().await;
        let mark = marks.entry(route.to_owned()).or_insert(until);
        *mark = (*mark).max(until);
    }

    /// 印を消す。通った (2xx) 経路に対して呼ぶ。
    pub async fn allow(&self, route: &str) {
        // 印が無いのが普通なので、書き込みの鍵を取る前に見る。
        if !self.until.read().await.contains_key(route) {
            return;
        }
        self.until.write().await.remove(route);
    }

    /// この経路が断られている期限。空いていれば `None`。
    pub async fn until(&self, route: &str, now: i64) -> Option<i64> {
        self.until
            .read()
            .await
            .get(route)
            .copied()
            .filter(|u| *u > now)
    }

    /// 試す順に並べ直す。
    ///
    /// 断られている経路は外す。ただし**全部が断られている場合は外さない** —
    /// 誰にも聞かずに 429 を返すより、実際に聞いて今の状況を確かめる方が良い。
    /// その場合は期限が最も近いものから試す (一番先に空いた見込みが高い)。
    pub async fn in_order(&self, routes: Vec<Arc<Route>>, now: i64) -> Vec<Arc<Route>> {
        let marks = self.until.read().await;
        let mark = |route: &Arc<Route>| marks.get(route.name()).copied().filter(|u| *u > now);

        let ready: Vec<Arc<Route>> = routes
            .iter()
            .filter(|r| mark(r).is_none())
            .cloned()
            .collect();
        if !ready.is_empty() {
            return ready;
        }

        let mut denied = routes;
        denied.sort_by_key(|r| mark(r).unwrap_or(now));
        denied
    }
}

/// この応答を受けて、いつまで空けるかを決める。
///
/// 手掛かりは近い順に見る:
///
/// 1. 上限のヘッダで**通らないと言われている窓**のうち、最も近いリセット時刻
///    (5 時間の窓が埋まっていても 7 日の窓に余裕があるなら、5 時間の方で空く)
/// 2. `retry-after` の秒数
/// 3. どちらも無ければ既定の [`DEFAULT_COOLDOWN`]
pub fn denied_until(headers: &[(String, String)], now: i64) -> i64 {
    if let Some(reset) = nearest_reset(headers, now) {
        return capped(reset, now);
    }
    if let Some(after) = retry_after(headers) {
        return capped(now + after.max(0), now);
    }
    capped(now + DEFAULT_COOLDOWN, now)
}

/// 通らないと言われている窓のうち、最も近いリセット時刻。
fn nearest_reset(headers: &[(String, String)], now: i64) -> Option<i64> {
    let snapshot = Snapshot::from_headers(headers, now)?;
    [snapshot.five_hour, snapshot.seven_day]
        .into_iter()
        .flatten()
        .filter(is_rejected)
        .filter_map(|w| w.reset)
        // 過ぎているリセット時刻は手掛かりにならない。次を見る。
        .filter(|reset| *reset > now)
        .min()
}

/// この窓は通らない状態か。
///
/// 語彙は公式に記載が無く増えうる (DR-0007) ので、`allowed` **以外**を
/// 通らない扱いにする。知らない語を「通る」側に倒すと、印が付かないまま
/// 毎回 429 を貰い続ける元の状態に戻る。
fn is_rejected(window: &Window) -> bool {
    window
        .status
        .as_deref()
        .is_some_and(|s| !s.trim().eq_ignore_ascii_case("allowed"))
}

/// `retry-after` の秒数。HTTP-date 形式なら読まない (次の手掛かりへ)。
fn retry_after(headers: &[(String, String)]) -> Option<i64> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// 上限で頭を押さえる。
fn capped(until: i64, now: i64) -> i64 {
    until.min(now + MAX_COOLDOWN)
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

    /// 上限のヘッダがあれば、通らない窓のリセット時刻で空ける。
    #[test]
    fn a_rejected_window_sets_the_deadline() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            (
                "anthropic-ratelimit-unified-5h-reset",
                &(NOW + 100).to_string(),
            ),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            (
                "anthropic-ratelimit-unified-7d-reset",
                &(NOW + 5000).to_string(),
            ),
        ]);
        assert_eq!(
            denied_until(&h, NOW),
            NOW + 5000,
            "通っている 5h ではなく、断っている 7d のリセットを見る"
        );
    }

    /// 通らない窓が複数あるなら、先に空く方まで。
    #[test]
    fn the_nearest_rejected_window_wins() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            (
                "anthropic-ratelimit-unified-5h-reset",
                &(NOW + 100).to_string(),
            ),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            (
                "anthropic-ratelimit-unified-7d-reset",
                &(NOW + 5000).to_string(),
            ),
        ]);
        assert_eq!(denied_until(&h, NOW), NOW + 100);
    }

    /// 上限のヘッダが無ければ `retry-after`。529 の多くはこちら。
    #[test]
    fn retry_after_is_the_second_hint() {
        let h = headers(&[("retry-after", "30")]);
        assert_eq!(denied_until(&h, NOW), NOW + 30);
    }

    /// 窓が全部通っている状態なら、リセット時刻は手掛かりにしない。
    #[test]
    fn an_allowed_window_is_not_a_hint() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            (
                "anthropic-ratelimit-unified-5h-reset",
                &(NOW + 5000).to_string(),
            ),
            ("retry-after", "30"),
        ]);
        assert_eq!(denied_until(&h, NOW), NOW + 30);
    }

    /// 何も無ければ既定値。
    #[test]
    fn without_hints_the_default_applies() {
        assert_eq!(denied_until(&[], NOW), NOW + DEFAULT_COOLDOWN);
    }

    /// 日付形式の `retry-after` は読まない。既定値に落とす。
    #[test]
    fn an_http_date_retry_after_falls_back_to_the_default() {
        let h = headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(denied_until(&h, NOW), NOW + DEFAULT_COOLDOWN);
    }

    /// 遠すぎるリセット時刻でも 24 時間で頭を押さえる。
    #[test]
    fn the_deadline_is_capped() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            (
                "anthropic-ratelimit-unified-7d-reset",
                &(NOW + 400 * 24 * 3600).to_string(),
            ),
        ]);
        assert_eq!(denied_until(&h, NOW), NOW + MAX_COOLDOWN);
    }

    /// 過ぎたリセット時刻は使わない。
    #[test]
    fn a_past_reset_is_ignored() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            (
                "anthropic-ratelimit-unified-7d-reset",
                &(NOW - 10).to_string(),
            ),
            ("retry-after", "30"),
        ]);
        assert_eq!(denied_until(&h, NOW), NOW + 30);
    }

    #[tokio::test]
    async fn a_mark_expires() {
        let d = Denials::new();
        d.deny("a", NOW + 10).await;
        assert_eq!(d.until("a", NOW).await, Some(NOW + 10));
        assert_eq!(d.until("a", NOW + 10).await, None, "期限は含まない");
    }

    /// 印は遠い方を残す。既定値で読めていたリセット時刻を潰さない。
    #[tokio::test]
    async fn the_later_deadline_is_kept() {
        let d = Denials::new();
        d.deny("a", NOW + 5000).await;
        d.deny("a", NOW + 60).await;
        assert_eq!(d.until("a", NOW).await, Some(NOW + 5000));
    }

    #[tokio::test]
    async fn success_clears_the_mark() {
        let d = Denials::new();
        d.deny("a", NOW + 5000).await;
        d.allow("a").await;
        assert_eq!(d.until("a", NOW).await, None);
    }
}
