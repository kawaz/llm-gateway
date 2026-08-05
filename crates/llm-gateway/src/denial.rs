//! 断られた経路を、しばらく試さないでおく (DR-0009)。
//!
//! 印が無いと、上限に当たった認証情報が先頭にいるだけで**すべてのリクエストが
//! 毎回そこに当たって 429 を貰い、次へ回る**。窓が開くまで、1 本あたり 1 往復
//! ぶんの遅れが乗り続ける。
//!
//! 断られ方で、空ける長さと**効かせる範囲**が変わる:
//!
//! - **上限** ([`Reason::Limited`]) — 429 と一緒に、どの窓 (5 時間 / 7 日) が
//!   塞がっていていつ開くかが返る。窓はアカウント全体に掛かるので、
//!   [`Scope::Everything`] (その credential の全モデル) で、開く時刻まで空ける
//! - **混雑** ([`Reason::Busy`]) — 529 や、窓を伴わない 429。いつ空くかは
//!   誰も知らないので短く退避するだけで、範囲も [`Scope::Model`]
//!   (頼んだモデルだけ) に絞る
//!
//! 印はメモリだけに持つ。落として失うのは再起動直後の 1 回の空振りで、その
//! 1 回が印を付け直す。ディスクに置くと、もう空いている経路を古い印で
//! 締め出す方向の間違いが増える。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::credential::time::parse_rfc3339;
use crate::limits::Limit;
use crate::quota::{Snapshot, Window};
use crate::router::Route;

/// いつ空くかの手掛かりが何も無いときに空ける間隔 (秒)。
const DEFAULT_BACKOFF: i64 = 60;

/// `retry-after` をどこまで信じるか (秒)。
///
/// 窓の開く時刻と違い、`retry-after` は根拠を確かめようがない。桁の壊れた値
/// (時刻の足し算が溢れる、実質永久に閉じる) をそのまま採ると、経路を失う。
const MAX_BACKOFF: i64 = 7 * 24 * 60 * 60;

/// 枠が開く時刻に足す猶予 (秒)。
///
/// 宣言されたリセット時刻ちょうどに使い始めても、**1 分ほど遅れて**実際に
/// 使えるようになる (kawaz 実測、5 時間の枠で毎回そうなる)。upstream が
/// 1 分程度の単位で枠を数え直しているとみられる。ちょうどに解放すると、
/// 直後の 1 本が高い確率で 429 を貰い、その 429 でまた締め出しが伸びる。
pub const RESET_SLACK: i64 = 60;

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

/// 締め出しが効く範囲。
///
/// 同じ credential でも、**あるモデルだけ断られる**ことが実際にある
/// (実測 2026-07-31: 同じアカウントに haiku は 200、fable / opus / sonnet は
/// 429)。断られたモデルの都合で他のモデルまで締め出すと、使える経路を
/// 自分で閉じることになる。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// この credential の全モデル。アカウント全体に掛かる枠が塞がっている場合。
    Everything,
    /// このモデルを頼んだときだけ。
    Model(String),
    /// この形に当てはまるモデルだけ (`claude-fable-*` のような glob)。
    ///
    /// 枠が「Fable の枠」のようにモデル**群**へ掛かる場合に使う。名前を
    /// 並べて持たないのは、個別枠が増えたときに設定も実装も直さずに済ませる
    /// ため (DR-0007)。
    Models(String),
}

impl Scope {
    /// この範囲は、このモデルに効くか。
    pub fn covers(&self, model: &str) -> bool {
        match self {
            Self::Everything => true,
            Self::Model(name) => name == model,
            Self::Models(pattern) => crate::pattern::matches(pattern, model),
        }
    }
}

/// この経路は、いつまで、どの範囲で断られているか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// この時刻 (Unix 秒) までは候補にしない。
    pub until: i64,
    pub reason: Reason,
    pub scope: Scope,
}

/// この応答は経路を締め出すか。するならいつまで、どの範囲で。
///
/// - **429 + 塞がっている窓** → その窓が開く時刻 (+ [`RESET_SLACK`]) まで、
///   credential 全体を ([`Reason::Limited`] / [`Scope::Everything`])。窓が
///   複数塞がっているなら**最も遅く開く時刻**まで — 5 時間の窓が開いても、
///   7 日の窓が塞がったままなら通らない
/// - **429 で窓が読めない / 529** → `retry-after`、無ければ
///   [`DEFAULT_BACKOFF`] だけ、頼んだモデルにだけ
///   ([`Reason::Busy`] / [`Scope::Model`])
/// - それ以外の状態 → 締め出さない。401 / 403 は待っても直らないので、
///   時間で空ける印を付ける意味がない
///
/// 範囲を応答から決めるのは、**モデル別の制限がヘッダに出てこない**ため
/// (実測 2026-07-31、DR-0009)。窓が塞がったという証拠があるときだけ
/// credential 全体に広げ、証拠が無いものは頼んだモデルの事情として扱う。
pub fn denial_of(
    status: u16,
    headers: &[(String, String)],
    model: &str,
    now: i64,
) -> Option<Denial> {
    if !matches!(status, 429 | 529) {
        return None;
    }
    if status == 429
        && let Some(reset) = last_reset(headers, now)
    {
        return Some(Denial {
            // 開くと言われた時刻ちょうどではなく、少し待ってから戻す。
            until: reset + RESET_SLACK,
            reason: Reason::Limited,
            scope: Scope::Everything,
        });
    }
    let after = retry_after(headers).unwrap_or(DEFAULT_BACKOFF);
    Some(Denial {
        until: now + after.clamp(0, MAX_BACKOFF),
        reason: Reason::Busy,
        scope: Scope::Model(model.to_owned()),
    })
}

/// 塞がっている窓が全部開くのはいつか。
///
/// **最も遅い方**を採る。5 時間の窓が開いても、7 日の窓が塞がったままなら
/// このリクエストは通らない。早い方を採ると、開いていない相手に当てに行って
/// 429 を貰い直すことになる。
fn last_reset(headers: &[(String, String)], now: i64) -> Option<i64> {
    let snapshot = Snapshot::from_headers(headers, now)?;
    [snapshot.five_hour, snapshot.seven_day]
        .into_iter()
        .flatten()
        .filter(is_rejected)
        .filter_map(|w| w.reset)
        // 過ぎている時刻は手掛かりにならない。次の手掛かりへ落とす。
        .filter(|reset| *reset > now)
        .max()
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
/// 鍵は経路の名前 (= 設定に書いた credential の名前) と範囲の組。認証情報を
/// 持たない経路 (relay) にも 529 は返るので、`CredentialId` ではなく経路の
/// 名前で持つ。
#[derive(Default)]
pub struct Denials {
    marks: Mutex<HashMap<Key, Denial>>,
    /// 経路ごとの「様子を聞きに行った」記録。
    ///
    /// 印とは別に持つ。**印が無い経路にも聞きに行く** (窓を伴わない 429 を
    /// 受けた直後など) ので、印にぶら下げると聞きに行けない相手ができる。
    asked: Mutex<HashMap<String, Ask>>,
}

/// 印の宛先。同じ経路でも、範囲が違えば別の印。
type Key = (String, Scope);

#[derive(Default)]
struct Ask {
    /// 最後にこちらから聞きに行った時刻。
    asked_at: i64,
    /// 最後にこの経路の状態を知った時刻 (断られた応答を含む)。
    heard_at: i64,
    /// いま聞いている最中。同じ相手に一斉に聞きに行かないための札。
    in_flight: bool,
}

impl Denials {
    pub fn new() -> Self {
        Self::default()
    }

    fn marks(&self) -> MutexGuard<'_, HashMap<Key, Denial>> {
        self.marks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn asked(&self) -> MutexGuard<'_, HashMap<String, Ask>> {
        self.asked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 断られたことを控える。
    ///
    /// 上限 ([`Reason::Limited`]) は枠の状態が正本なので、そのまま置く。
    /// 混雑 ([`Reason::Busy`]) は当て推量なので、**同じ宛先に先に控えた期限を
    /// 縮めない** — 60 秒の退避で、読めていた枠の開く時刻を上書きしない。
    pub fn deny(&self, route: &str, denial: Denial, now: i64) {
        let mut marks = self.marks();
        // 期限の切れた印を落とす。モデルごとに印が増えるので、掃除しないと
        // 使われなくなったモデル名が溜まり続ける。
        marks.retain(|_, d| d.until > now);

        let key = (route.to_owned(), denial.scope.clone());
        let denial = match marks.get(&key) {
            Some(kept) if denial.reason == Reason::Busy && kept.until >= denial.until => {
                kept.clone()
            }
            _ => denial,
        };
        marks.insert(key, denial);
        drop(marks);

        // 断られたのも「状態を知った」うち。定期の様子見はここから測る。
        self.asked().entry(route.to_owned()).or_default().heard_at = now;
    }

    /// 印を消す。通った (2xx) 経路に対して呼ぶ。
    ///
    /// このモデルで通ったのだから、**このモデルに効いていた印は全部**間違い
    /// だったことになる。credential 全体の印も、モデルの形で括った印も消す。
    ///
    /// Design rationale: 裏で聞きに行った結果と、実際の転送の結果が前後する
    /// のを防ぐ仕掛けは置かない。古い観測で誤って印を外しても、次の実
    /// リクエストが 1 往復で 429 を貰って付け直す。古い観測で誤って印を
    /// 付けても、次に通った応答が消す。どちらも 1 リクエストで直るので、
    /// 順序を守るための仕掛け (世代番号・観測時刻の比較) を足すと、得られる
    /// ものより仕掛けの複雑さの方が大きい。
    pub fn allow(&self, route: &str, model: &str) {
        self.marks()
            .retain(|(name, scope), _| name != route || !scope.covers(model));
    }

    /// この範囲の印だけを消す。枠が塞がっていないと分かったときに呼ぶ。
    pub fn clear(&self, route: &str, scope: &Scope) {
        self.marks().remove(&(route.to_owned(), scope.clone()));
    }

    /// この経路をこのモデルで使えるか。断られていれば、その印。
    ///
    /// このモデルに効く印が複数あるなら**遅く開く方**を返す — この経路が
    /// 使えるようになるのは全部が開いた時なので、早い方を返すとまだ塞がって
    /// いる経路を「もう開く」と伝えることになる。
    pub fn get(&self, route: &str, model: &str, now: i64) -> Option<Denial> {
        self.marks()
            .iter()
            .filter(|((name, scope), _)| name == route && scope.covers(model))
            .map(|(_, denial)| denial.clone())
            .filter(|d| d.until > now)
            .max_by_key(|d| d.until)
    }

    /// 今このリクエストで試せる経路を選ぶ。
    ///
    /// 断られている経路は**外す**。開く時刻を知っていながら実リクエストを
    /// 当てるのは、分かっている壁にわざわざぶつかりに行くのと同じ。
    pub fn candidates(&self, routes: &[Arc<Route>], model: &str, now: i64) -> Candidates {
        let denied: Vec<Option<Denial>> = routes
            .iter()
            .map(|r| self.get(r.name(), model, now))
            .collect();

        let ready: Vec<Arc<Route>> = routes
            .iter()
            .zip(&denied)
            .filter(|(_, d)| d.is_none())
            .map(|(r, _)| Arc::clone(r))
            .collect();
        if !ready.is_empty() {
            return Candidates::Ready(ready);
        }
        Candidates::AllDenied {
            // どれも塞がっているなら、最初に開くのがいつかがクライアントの知りたいこと。
            until: denied
                .into_iter()
                .flatten()
                .map(|d| d.until)
                .min()
                .unwrap_or(now),
        }
    }

    /// 枠の一覧を聞けたので、この経路の印を引き直す ([`crate::limits`])。
    ///
    /// 使い切った枠 (`percent` が 100 以上) は、開く時刻まで締め出す。
    /// 使い切っていない枠は、その範囲の印を消す。**`is_active` は見ない** —
    /// 実測 (2026-08-01) では 47 % の枠が `is_active: true` を返す。あれは
    /// 「今どの枠を見ているか」であって「塞がっている」ではない。
    ///
    /// いつ開くか (`resets_at`) が分からない枠では印を作らない。期限を
    /// 決められないものを締め出すと、いつ戻すかも決められない。
    pub fn apply_limits(&self, route: &str, limits: &[Limit], now: i64) {
        for limit in limits {
            let scope = scope_of(limit);
            if limit.percent < EXHAUSTED {
                self.clear(route, &scope);
                continue;
            }
            let Some(reset) = limit.resets_at.as_deref().and_then(parse_rfc3339) else {
                continue;
            };
            self.deny(
                route,
                Denial {
                    until: reset + RESET_SLACK,
                    reason: Reason::Limited,
                    scope,
                },
                now,
            );
        }
    }

    /// 定期の様子見の役を引き受ける。引き受けられたら札を返す。
    ///
    /// 引き受けるのは、上限で締め出している経路だけ。混雑はいつ空くか
    /// 分からないので短い退避で足り、定期的に聞いても実費が増えるだけになる。
    pub fn claim_probe(self: &Arc<Self>, route: &str, now: i64) -> Option<Probing> {
        let limited = self
            .marks()
            .iter()
            .any(|((name, _), d)| name == route && d.reason == Reason::Limited && d.until > now);
        if !limited {
            return None;
        }
        // 直前に断られたばかりなら、その応答が最新の観測。すぐ聞き直さない。
        let heard_at = self.asked().get(route).map_or(0, |a| a.heard_at);
        if now - heard_at < PROBE_INTERVAL {
            return None;
        }
        self.claim(route, now, PROBE_INTERVAL)
    }

    /// 間隔を待たずに引き受ける。
    ///
    /// どの経路も断られたとき、または窓を伴わない 429 を受けたときに使う。
    /// どちらも「今の枠を知りたい」瞬間で、次の周期を待つ理由がない。
    ///
    /// それでも [`DEFAULT_BACKOFF`] だけは空ける。窓を伴わない断りはその間
    /// どうせ締め出されているので、続けて聞いても結果を使う先がない。
    pub fn claim_ask(self: &Arc<Self>, route: &str, now: i64) -> Option<Probing> {
        self.claim(route, now, DEFAULT_BACKOFF)
    }

    fn claim(self: &Arc<Self>, route: &str, now: i64, interval: i64) -> Option<Probing> {
        let mut asked = self.asked();
        let ask = asked.entry(route.to_owned()).or_default();
        if ask.in_flight || (ask.asked_at != 0 && now - ask.asked_at < interval) {
            return None;
        }
        ask.in_flight = true;
        ask.asked_at = now;
        ask.heard_at = now;
        Some(Probing {
            denials: Arc::clone(self),
            route: route.to_owned(),
        })
    }
}

/// 使い切ったとみなす使用率。
const EXHAUSTED: f64 = 100.0;

/// この枠はどの範囲に効くか。
///
/// モデルが付かない枠 (`session` / `weekly_all`) はアカウント全体。モデルが
/// 付く枠 (`weekly_scoped`) はそのモデル群だけ。識別子が返るならそれで
/// 一致を見て、返らないなら表示名から形を起こす。
fn scope_of(limit: &Limit) -> Scope {
    if let Some(id) = &limit.model_id {
        return Scope::Model(id.clone());
    }
    match &limit.model {
        Some(name) => Scope::Models(pattern_for(name)),
        None => Scope::Everything,
    }
}

/// 表示名からモデルの形を起こす。`Fable` → `claude-fable-*`。
///
/// 名前を並べて持たない (`fable` だけ特別扱いする等をしない) のは、個別枠が
/// 増えたときに実装を直さずに済ませるため。
fn pattern_for(display_name: &str) -> String {
    let name = display_name
        .trim()
        .to_lowercase()
        .replace(char::is_whitespace, "-");
    if name.starts_with("claude") {
        format!("{name}-*")
    } else {
        format!("claude-{name}-*")
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
        if let Some(ask) = self.denials.asked().get_mut(&self.route) {
            ask.in_flight = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    /// 試験で使うモデル名。実際の運用と同じく、断られ方がモデルで違う。
    const FABLE: &str = "claude-fable-5";
    const HAIKU: &str = "claude-haiku-4-5-20251001";

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
            scope: Scope::Everything,
        }
    }

    fn busy(until: i64, model: &str) -> Denial {
        Denial {
            until,
            reason: Reason::Busy,
            scope: Scope::Model(model.to_owned()),
        }
    }

    /// 塞がっている窓があれば、そこが開く時刻まで credential 全体を外す。
    #[test]
    fn a_rejected_window_closes_the_whole_credential() {
        let mut h = window("5h", "allowed", NOW + 100);
        h.extend(window("7d", "rejected", NOW + 5000));
        assert_eq!(
            denial_of(429, &h, FABLE, NOW),
            Some(limited(NOW + 5000 + RESET_SLACK)),
            "窓はアカウントに掛かるので、頼んだモデルには関係しない"
        );
    }

    /// 開くと言われた時刻ちょうどには戻さない。
    ///
    /// リセット時刻ちょうどに使い始めても 1 分ほど遅れて実際に使えるように
    /// なる (kawaz 実測)。ちょうどに戻すと、その 1 本が 429 を貰って
    /// 締め出しが伸びる。
    #[test]
    fn the_route_comes_back_a_little_after_the_reset() {
        let h = window("7d", "rejected", NOW + 5000);
        let denial = denial_of(429, &h, FABLE, NOW).unwrap();
        assert_eq!(denial.until, NOW + 5000 + RESET_SLACK);
        assert!(
            denial.until > NOW + 5000,
            "開くと言われた時刻より後に戻す (猶予 {} 秒)",
            denial.until - (NOW + 5000)
        );
    }

    /// 塞がっている窓が複数あるなら、全部開くまで。
    ///
    /// 5 時間の窓が開いても、7 日の窓が塞がったままなら通らない。早い方を
    /// 採ると、開いていない相手に当てに行って 429 を貰い直すことになる。
    #[test]
    fn every_rejected_window_must_open() {
        let mut h = window("5h", "rejected", NOW + 100);
        h.extend(window("7d", "rejected", NOW + 5000));
        assert_eq!(
            denial_of(429, &h, FABLE, NOW),
            Some(limited(NOW + 5000 + RESET_SLACK))
        );
    }

    /// 開いている窓の時刻は数えない。塞がっている窓だけを見る。
    #[test]
    fn an_open_window_does_not_extend_the_deadline() {
        let mut h = window("5h", "rejected", NOW + 100);
        h.extend(window("7d", "allowed", NOW + 5000));
        assert_eq!(
            denial_of(429, &h, FABLE, NOW),
            Some(limited(NOW + 100 + RESET_SLACK))
        );
    }

    /// 遠い開く時刻でも縮めない。開く時刻を知っているならそこまで待つ。
    #[test]
    fn a_distant_reset_is_not_shortened() {
        let h = window("7d", "rejected", NOW + 400 * 24 * 3600);
        assert_eq!(
            denial_of(429, &h, FABLE, NOW),
            Some(limited(NOW + 400 * 24 * 3600 + RESET_SLACK))
        );
    }

    /// 警告つきでも通ってはいる。塞がっている扱いにしない。
    #[test]
    fn a_warning_window_is_still_open() {
        let mut h = window("5h", "allowed_warning", NOW + 100);
        h.extend(headers(&[("retry-after", "30")]));
        assert_eq!(
            denial_of(429, &h, FABLE, NOW),
            Some(busy(NOW + 30, FABLE)),
            "窓は塞がっていないので、頼んだモデルの事情として短く退避する"
        );
    }

    /// 窓が読めない 429 は、頼んだモデルだけを短く外す。
    ///
    /// 実測 (2026-07-31): 同じ credential で haiku は 200 (全窓 allowed)、
    /// fable / opus / sonnet は上限のヘッダを 1 つも載せない 429 を返す。
    /// モデル別の制限はヘッダに出てこないので、頼んだモデル以外に広げる
    /// 根拠がない。
    #[test]
    fn a_bare_rate_limit_only_closes_the_model_asked_for() {
        assert_eq!(
            denial_of(429, &[], FABLE, NOW),
            Some(busy(NOW + DEFAULT_BACKOFF, FABLE))
        );
        assert_eq!(
            denial_of(429, &headers(&[("retry-after", "30")]), FABLE, NOW),
            Some(busy(NOW + 30, FABLE))
        );
    }

    /// 529 も宛先とモデルの都合。窓のヘッダが載っていても短い退避のまま。
    #[test]
    fn an_overloaded_upstream_only_steps_aside() {
        let h = window("7d", "rejected", NOW + 5000);
        assert_eq!(
            denial_of(529, &h, FABLE, NOW),
            Some(busy(NOW + DEFAULT_BACKOFF, FABLE))
        );
    }

    /// 過ぎた開く時刻は使わない。
    #[test]
    fn a_past_reset_is_ignored() {
        let mut h = window("7d", "rejected", NOW - 10);
        h.extend(headers(&[("retry-after", "30")]));
        assert_eq!(denial_of(429, &h, FABLE, NOW), Some(busy(NOW + 30, FABLE)));
    }

    /// 日付形式の `retry-after` は読まない。
    #[test]
    fn an_http_date_retry_after_falls_back_to_the_default() {
        let h = headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(
            denial_of(529, &h, FABLE, NOW),
            Some(busy(NOW + DEFAULT_BACKOFF, FABLE))
        );
    }

    /// 待っても直らない断りは締め出さない。
    #[test]
    fn an_auth_failure_is_not_a_cooldown() {
        for status in [200, 400, 401, 403, 500] {
            assert_eq!(denial_of(status, &[], FABLE, NOW), None, "{status}");
        }
    }

    /// あるモデルで断られても、同じ credential の他のモデルは使える。
    ///
    /// これを分けないと、fable の 429 だけで haiku まで止まる。
    #[test]
    fn one_model_denial_leaves_the_others_alone() {
        let d = Denials::new();
        d.deny("a", busy(NOW + 60, FABLE), NOW);
        assert_eq!(d.get("a", FABLE, NOW), Some(busy(NOW + 60, FABLE)));
        assert_eq!(d.get("a", HAIKU, NOW), None);
    }

    /// 窓が塞がっているなら、どのモデルでも使えない。
    #[test]
    fn a_closed_window_stops_every_model() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 5000), NOW);
        for model in [FABLE, HAIKU] {
            assert_eq!(d.get("a", model, NOW), Some(limited(NOW + 5000)), "{model}");
        }
    }

    #[test]
    fn a_mark_expires() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 10), NOW);
        assert_eq!(d.get("a", FABLE, NOW), Some(limited(NOW + 10)));
        assert_eq!(d.get("a", FABLE, NOW + 10), None, "期限は含まない");
    }

    /// 短い退避で、読めていた開く時刻を潰さない。
    #[test]
    fn congestion_does_not_shorten_a_known_deadline() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 5000), NOW);
        d.deny("a", busy(NOW + 60, FABLE), NOW);
        assert_eq!(
            d.get("a", HAIKU, NOW),
            Some(limited(NOW + 5000)),
            "別の宛先の印なので、窓の期限はそのまま残る"
        );
    }

    /// 同じ宛先への短い退避は、期限を縮めない。
    #[test]
    fn a_shorter_backoff_does_not_replace_a_longer_one() {
        let d = Denials::new();
        d.deny("a", busy(NOW + 300, FABLE), NOW);
        d.deny("a", busy(NOW + 60, FABLE), NOW);
        assert_eq!(d.get("a", FABLE, NOW), Some(busy(NOW + 300, FABLE)));
    }

    /// 上限のヘッダは正本。前より近い時刻でも、そのまま置く。
    #[test]
    fn a_fresh_limit_replaces_the_deadline() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 5000), NOW);
        d.deny("a", limited(NOW + 100), NOW);
        assert_eq!(d.get("a", FABLE, NOW), Some(limited(NOW + 100)));
    }

    /// このモデルで通ったなら、そのモデルの印も credential 全体の印も外す。
    #[test]
    fn success_clears_both_marks() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 5000), NOW);
        d.deny("a", busy(NOW + 60, FABLE), NOW);
        d.allow("a", FABLE);
        assert_eq!(d.get("a", FABLE, NOW), None);
        assert_eq!(d.get("a", HAIKU, NOW), None);
    }

    /// 別のモデルで通っても、断られているモデルの印までは外さない。
    #[test]
    fn success_with_another_model_keeps_the_model_mark() {
        let d = Denials::new();
        d.deny("a", busy(NOW + 60, FABLE), NOW);
        d.allow("a", HAIKU);
        assert_eq!(d.get("a", FABLE, NOW), Some(busy(NOW + 60, FABLE)));
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

    /// 間隔が空くまでは聞きに行かない。モデル別の短い退避には聞きに行かない。
    #[test]
    fn probing_waits_for_the_interval_and_skips_congestion() {
        let d = Arc::new(Denials::new());
        d.deny("a", limited(NOW + 100_000), NOW);
        assert!(d.claim_probe("a", NOW + PROBE_INTERVAL - 1).is_none());
        assert!(d.claim_probe("a", NOW + PROBE_INTERVAL).is_some());

        d.deny("b", busy(NOW + 100_000, FABLE), NOW);
        assert!(
            d.claim_probe("b", NOW + PROBE_INTERVAL).is_none(),
            "いつ空くか分からない相手に定期的に聞いても、実費が増えるだけ"
        );
    }

    /// 桁の壊れた `retry-after` で時刻の足し算を溢れさせない。
    #[test]
    fn an_absurd_retry_after_is_clamped() {
        for value in [i64::MAX.to_string(), "999999999999".to_owned()] {
            let h = headers(&[("retry-after", &value)]);
            assert_eq!(
                denial_of(429, &h, FABLE, NOW),
                Some(busy(NOW + MAX_BACKOFF, FABLE)),
                "{value}"
            );
        }
    }

    /// 負の `retry-after` は 0 扱い。過去の時刻を印にしない。
    #[test]
    fn a_negative_retry_after_does_not_look_expired() {
        let h = headers(&[("retry-after", "-10")]);
        assert_eq!(denial_of(429, &h, FABLE, NOW), Some(busy(NOW, FABLE)));
    }

    /// 印が 2 つ生きているなら、この経路が使えるのは遅い方が開いた時。
    #[test]
    fn the_later_of_two_marks_decides_when_the_route_returns() {
        let d = Denials::new();
        d.deny("a", limited(NOW + 100), NOW);
        d.deny("a", busy(NOW + 5000, FABLE), NOW);
        assert_eq!(
            d.get("a", FABLE, NOW),
            Some(busy(NOW + 5000, FABLE)),
            "窓が先に開いても、モデルの印が残っていれば使えない"
        );
        assert_eq!(
            d.get("a", HAIKU, NOW),
            Some(limited(NOW + 100)),
            "そのモデルの印が無い側は窓だけ見る"
        );
    }

    /// 期限の切れた印は溜めない。モデル名ごとに印が増えるので、放っておくと
    /// 使われなくなったモデルの分が残り続ける。
    #[test]
    fn expired_marks_are_swept() {
        let d = Denials::new();
        for model in ["m1", "m2", "m3"] {
            d.deny("a", busy(NOW + 60, model), NOW);
        }
        assert_eq!(d.marks().len(), 3);

        let later = NOW + 61;
        d.deny("a", busy(later + 60, "m4"), later);
        assert_eq!(d.marks().len(), 1, "生きているのは今付けた 1 つだけ");
    }

    /// 定期の様子見は、上限で締め出している経路にだけ立てる。
    #[test]
    fn only_a_limited_route_is_probed_on_a_schedule() {
        let d = Arc::new(Denials::new());
        d.deny("a", limited(NOW + 100_000), NOW);
        d.deny("b", busy(NOW + 100_000, FABLE), NOW);

        let later = NOW + PROBE_INTERVAL;
        assert!(d.claim_probe("a", later).is_some());
        assert!(
            d.claim_probe("b", later).is_none(),
            "いつ空くか分からない相手に定期的に聞いても、実費が増えるだけ"
        );
    }

    /// 印が無い経路にも、急ぎなら聞きに行ける。
    ///
    /// 窓を伴わない 429 を受けた直後がこれ。印は当て推量の 60 秒しか無く、
    /// **その中身を確かめるために聞く**。
    #[test]
    fn an_urgent_ask_does_not_need_a_mark() {
        let d = Arc::new(Denials::new());
        let held = d.claim_ask("a", NOW).expect("印が無くても引き受ける");
        assert_eq!(held.route(), "a");
        assert!(
            d.claim_ask("a", NOW).is_none(),
            "聞いている最中は誰も割り込まない"
        );
        drop(held);
    }

    /// 急ぎでも、続けざまには聞かない。
    ///
    /// 窓を伴わない断りはその間どうせ締め出されているので、続けて聞いても
    /// 結果を使う先がない。
    #[test]
    fn an_urgent_ask_still_leaves_a_gap() {
        let d = Arc::new(Denials::new());
        drop(d.claim_ask("a", NOW).expect("1 回目"));
        assert!(d.claim_ask("a", NOW + DEFAULT_BACKOFF - 1).is_none());
        assert!(d.claim_ask("a", NOW + DEFAULT_BACKOFF).is_some());
    }

    #[test]
    fn an_unmarked_route_is_not_probed_on_a_schedule() {
        let d = Arc::new(Denials::new());
        assert!(d.claim_probe("a", NOW).is_none());
    }

    fn limit(kind: &str, percent: f64, model: Option<&str>, resets_at: Option<&str>) -> Limit {
        Limit {
            kind: kind.to_owned(),
            percent,
            severity: None,
            resets_at: resets_at.map(str::to_owned),
            model: model.map(str::to_owned),
            model_id: None,
            // 実測どおり、塞がっていない枠でも立つ。判断には使わない。
            is_active: true,
        }
    }

    /// NOW の 5000 秒後。枠の開く時刻は小数秒つきで返ってくる。
    const RESET_ISO: &str = "2027-01-15T09:23:20.571539+00:00";
    const RESET: i64 = NOW + 5000;

    /// アカウント全体に掛かる枠を使い切ったら、全モデルを締め出す。
    #[test]
    fn an_exhausted_account_limit_closes_every_model() {
        let d = Denials::new();
        d.apply_limits(
            "a",
            &[limit("weekly_all", 100.0, None, Some(RESET_ISO))],
            NOW,
        );
        for model in [FABLE, HAIKU] {
            assert_eq!(
                d.get("a", model, NOW),
                Some(limited(RESET + RESET_SLACK)),
                "{model}"
            );
        }
    }

    /// モデル別の枠を使い切ったら、そのモデル群だけを締め出す。
    #[test]
    fn an_exhausted_scoped_limit_closes_only_its_models() {
        let d = Denials::new();
        d.apply_limits(
            "a",
            &[limit(
                "weekly_scoped",
                100.0,
                Some("Fable"),
                Some(RESET_ISO),
            )],
            NOW,
        );
        assert_eq!(
            d.get("a", FABLE, NOW),
            Some(Denial {
                until: RESET + RESET_SLACK,
                reason: Reason::Limited,
                scope: Scope::Models("claude-fable-*".to_owned()),
            })
        );
        assert_eq!(d.get("a", HAIKU, NOW), None, "他のモデルは巻き込まない");
    }

    /// 使い切っていない枠は、その範囲の印を消す。
    #[test]
    fn a_limit_below_the_line_reopens_its_scope() {
        let d = Denials::new();
        let exhausted = [
            limit("weekly_all", 100.0, None, Some(RESET_ISO)),
            limit("weekly_scoped", 100.0, Some("Fable"), Some(RESET_ISO)),
        ];
        d.apply_limits("a", &exhausted, NOW);
        assert!(d.get("a", FABLE, NOW).is_some());

        let opened = [
            limit("weekly_all", 99.9, None, Some(RESET_ISO)),
            limit("weekly_scoped", 0.0, Some("Fable"), Some(RESET_ISO)),
        ];
        d.apply_limits("a", &opened, NOW);
        assert_eq!(d.get("a", FABLE, NOW), None);
        assert_eq!(d.get("a", HAIKU, NOW), None);
    }

    /// 片方だけ開いても、もう片方が塞がっていれば締め出しは残る。
    #[test]
    fn reopening_one_scope_leaves_the_other_closed() {
        let d = Denials::new();
        d.apply_limits(
            "a",
            &[
                limit("weekly_all", 100.0, None, Some(RESET_ISO)),
                limit("weekly_scoped", 100.0, Some("Fable"), Some(RESET_ISO)),
            ],
            NOW,
        );
        d.apply_limits(
            "a",
            &[limit("weekly_all", 12.0, None, Some(RESET_ISO))],
            NOW,
        );

        assert!(d.get("a", FABLE, NOW).is_some(), "Fable の枠は塞がったまま");
        assert_eq!(d.get("a", HAIKU, NOW), None, "全体の枠は開いた");
    }

    /// いつ開くか分からない枠では締め出さない。戻す時刻を決められない。
    #[test]
    fn an_exhausted_limit_without_a_reset_is_not_used() {
        let d = Denials::new();
        d.apply_limits("a", &[limit("session", 100.0, None, None)], NOW);
        assert_eq!(d.get("a", FABLE, NOW), None);
    }

    /// 通ったモデルに効いていた印は、形で括った分も含めて全部消す。
    #[test]
    fn success_clears_the_pattern_that_covered_the_model() {
        let d = Denials::new();
        d.apply_limits(
            "a",
            &[limit(
                "weekly_scoped",
                100.0,
                Some("Fable"),
                Some(RESET_ISO),
            )],
            NOW,
        );
        d.allow("a", FABLE);
        assert_eq!(d.get("a", FABLE, NOW), None);
    }

    /// 表示名からモデルの形を起こす。名前を並べて持たない。
    #[test]
    fn a_display_name_becomes_a_model_pattern() {
        assert_eq!(pattern_for("Fable"), "claude-fable-*");
        assert_eq!(pattern_for(" Opus "), "claude-opus-*");
        assert_eq!(pattern_for("Claude Sonnet"), "claude-sonnet-*");
        assert!(Scope::Models(pattern_for("Fable")).covers(FABLE));
        assert!(!Scope::Models(pattern_for("Fable")).covers(HAIKU));
    }

    /// 識別子が返るなら、それで一致を見る (今の実測では返らない)。
    #[test]
    fn an_identifier_is_matched_exactly() {
        let mut with_id = limit("weekly_scoped", 100.0, Some("Fable"), Some(RESET_ISO));
        with_id.model_id = Some(FABLE.to_owned());

        let d = Denials::new();
        d.apply_limits("a", &[with_id], NOW);
        assert!(d.get("a", FABLE, NOW).is_some());
        assert_eq!(d.get("a", "claude-fable-6", NOW), None, "完全一致で見る");
    }
}
