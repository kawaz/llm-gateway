//! 会話 (session) ごとの cache / keepalive の状態の答え (DR-0042)。
//!
//! 答えは**控えと経路の印の写し**で、問われた時点での判定 (期限が過ぎて
//! いるか、経路が使えるか) はしない。読む側が時刻を壁時計と比べる。欄の名前と
//! 意味は知らせ (DR-0012) と usage の `denials` (DR-0020) から借りる。時刻は
//! 全て Unix ミリ秒。
//!
//! 本文は載せない。控えには送出直前の本文が丸ごと入っているが、ここへ来る
//! のは本文を読み飛ばした [`KeptMeta`] だけ。

use std::cmp::Reverse;

use serde::{Deserialize, Serialize};

use crate::cache::keepalive::{Chain, KeptMeta};

/// 1 つの会話の答え (`GET /llm-gateway/sessions/{session_id}`)。
///
/// 控えの無い会話は `series` が空。gateway は会話そのものを知らないので、
/// 「無い会話」と「控えの無い会話」を区別しない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReport {
    pub session_id: String,
    /// `cache_since` の降順。先頭が最新の系列。
    pub series: Vec<SeriesReport>,
}

/// 控えのある会話の一覧 (`GET /llm-gateway/sessions`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsReport {
    /// 各会話の先頭の系列の `cache_since` の降順。
    pub sessions: Vec<SessionReport>,
}

/// 系列 1 つ ((session_id, prefix) の控え 1 つ) の写し。
///
/// `routes` 以外の欄は必ず出す。欠けさせると読む側が形を 2 通り扱う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesReport {
    pub prefix: String,
    pub ns: String,
    pub model: String,
    /// 直前の実リクエストが通った経路の名前。
    pub route: String,
    /// 最後に約束した寿命の名前。欄を持たない古い控えでは `null`。
    pub cache_notice: Option<String>,
    /// gateway が送り直す直前に壁時計と比べる期限 (余裕の 30 秒を引いた値)。
    pub cache_expires_at: i64,
    pub cache_since: i64,
    pub cache_count: u32,
    /// 控えに書いてある次の送り直しの予定 (格子の値ではない)。
    pub next_keepalive_at: i64,
    pub cache_until: i64,
    pub cache_until_count: u32,
    /// この系列の `ns` と `model` で試される経路を、試す順に。
    pub routes: Vec<RouteReport>,
}

/// 系列が試す経路 1 つと、それに付いている上限の印。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteReport {
    /// 経路の名前 (`[routes.<name>]`)。
    pub route: String,
    /// 経路が使う認証情報の名前。持たない経路 (relay) では欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// この `model` に効く上限の印。無ければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limited: Option<Limited>,
}

/// 上限の印 (`Reason::Limited`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limited {
    /// この時刻 (Unix ミリ秒) まで候補にしない。
    pub until: i64,
}

impl SeriesReport {
    /// 控え 1 つと、その系列が試す経路から組む。
    pub fn new(kept: KeptMeta, routes: Vec<RouteReport>) -> Self {
        let chain = Chain::of_kept(&kept);
        Self {
            prefix: kept.prefix,
            ns: kept.ns,
            model: kept.model,
            route: kept.route,
            cache_notice: kept.cache_notice,
            cache_expires_at: kept.expires_at_ms,
            cache_since: kept.since_ms,
            cache_count: kept.count,
            next_keepalive_at: kept.fires_at_ms,
            cache_until: chain.until_ms,
            cache_until_count: chain.until_count,
            routes,
        }
    }
}

impl SessionReport {
    /// 系列を `cache_since` の降順に並べて組む。
    ///
    /// 同じ時刻の系列は `prefix` の昇順に置く — 置き場を読む順は決まって
    /// いないので、揃えないと問うたびに並びが揺れる。
    pub fn new(session_id: String, mut series: Vec<SeriesReport>) -> Self {
        series.sort_by(|a, b| {
            (Reverse(a.cache_since), &a.prefix).cmp(&(Reverse(b.cache_since), &b.prefix))
        });
        Self { session_id, series }
    }

    /// 先頭 (最新) の系列の起点。系列が無ければ `None`。
    fn latest(&self) -> Option<i64> {
        self.series.first().map(|series| series.cache_since)
    }
}

impl SessionsReport {
    /// 系列を 1 つも持たない会話を落とし、先頭の系列の `cache_since` の降順に
    /// 並べて組む。同じ時刻の会話は `session_id` の昇順。
    pub fn new(sessions: impl IntoIterator<Item = SessionReport>) -> Self {
        let mut sessions: Vec<SessionReport> = sessions
            .into_iter()
            .filter(|session| !session.series.is_empty())
            .collect();
        sessions.sort_by(|a, b| {
            (Reverse(a.latest()), &a.session_id).cmp(&(Reverse(b.latest()), &b.session_id))
        });
        Self { sessions }
    }
}
