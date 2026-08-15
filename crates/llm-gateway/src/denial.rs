//! 1 経路が今どうなっているかの状態機構 (DR-0009 / DR-0014 §3)。
//!
//! core が規定するのは**型と契約**だけで、状態そのものは preset が持つ
//! ([`crate::provider::Preset`] の 1 フィールド)。断られ方の読み方や枠の
//! 解釈は provider ごとに違うので、そちらは preset 側にある。
//!
//! 印が無いと、上限に当たった認証情報が先頭にいるだけで**すべてのリクエストが
//! 毎回そこに当たって 429 を貰い、次へ回る**。窓が開くまで、1 本あたり 1 往復
//! ぶんの遅れが乗り続ける。
//!
//! 断られ方で、空ける長さと**効かせる範囲**が変わる:
//!
//! - **上限** ([`Reason::Limited`]) — どの窓が塞がっていていつ開くかが分かる
//!   場合。窓はアカウント全体に掛かるので [`Scope::Everything`] で、開く時刻
//!   まで空ける
//! - **混雑** ([`Reason::Busy`]) — いつ空くかは誰も知らないので短く退避する
//!   だけで、範囲も [`Scope::Model`] (頼んだモデルだけ) に絞る
//!
//! 印はメモリだけに持つ。落として失うのは再起動直後の 1 回の空振りで、その
//! 1 回が印を付け直す。ディスクに置くと、もう空いている経路を古い印で
//! 締め出す方向の間違いが増える。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::quota::Snapshot;

/// いつ空くかの手掛かりが何も無いときに空ける間隔 (秒)。
pub const DEFAULT_BACKOFF: i64 = 60;

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
/// 同じ経路でも、**あるモデルだけ断られる**ことが実際にある
/// (実測 2026-07-31: 同じアカウントに haiku は 200、fable / opus / sonnet は
/// 429)。断られたモデルの都合で他のモデルまで締め出すと、使える経路を
/// 自分で閉じることになる。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// この経路の全モデル。アカウント全体に掛かる枠が塞がっている場合。
    Everything,
    /// このモデルを頼んだときだけ。
    Model(String),
    /// この形に当てはまるモデルだけ (`*` を含む glob)。
    ///
    /// 枠がモデル**群**へ掛かる場合に使う。名前を並べて持たないのは、個別枠が
    /// 増えたときに設定も実装も直さずに済ませるため (DR-0007)。
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

/// この経路を今使えるか。
///
/// 印そのものを外へ配らない。呼び出し側 (router) が要るのは「使えるか」と
/// 「駄目なら次はいつか」だけで、どの範囲のどんな印が効いているかは経路の
/// 持ち物 (DR-0014 §3 の tell-don't-ask)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// 今すぐ試せる。
    Ready,
    /// この時刻 (Unix 秒) までは断られている。
    Denied { until: i64 },
}

/// 1 経路の状態。断られた印・枠のスナップショット・様子見の予定。
///
/// **経路の名前を鍵に持たない**。この状態を持っているのがその経路自身なので、
/// 「どの経路の話か」を文字列から復元する必要がない (DR-0014 §3)。
#[derive(Default)]
pub struct RouteState {
    /// 効いている印。範囲ごとに 1 つ。
    marks: Mutex<HashMap<Scope, Denial>>,
    /// 最後に観測した枠。
    quota: Mutex<Option<Snapshot>>,
    /// 様子を聞きに行った記録。
    ///
    /// 印とは別に持つ。**印が無い経路にも聞きに行く** (窓を伴わない 429 を
    /// 受けた直後など) ので、印にぶら下げると聞きに行けない相手ができる。
    /// [`Probing`] が札を返すので、鍵の実体を共有する。
    ask: Arc<Mutex<Ask>>,
}

#[derive(Default)]
struct Ask {
    /// 最後にこちらから聞きに行った時刻。
    asked_at: i64,
    /// 最後にこの経路の状態を知った時刻 (断られた応答を含む)。
    heard_at: i64,
    /// いま聞いている最中。同じ相手に一斉に聞きに行かないための札。
    in_flight: bool,
}

impl RouteState {
    pub fn new() -> Self {
        Self::default()
    }

    fn marks(&self) -> MutexGuard<'_, HashMap<Scope, Denial>> {
        self.marks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ask(&self) -> MutexGuard<'_, Ask> {
        self.ask.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 断られたことを控える。
    ///
    /// 上限 ([`Reason::Limited`]) は枠の状態が正本なので、そのまま置く。
    /// 混雑 ([`Reason::Busy`]) は当て推量なので、**同じ範囲に先に控えた期限を
    /// 縮めない** — 60 秒の退避で、読めていた枠の開く時刻を上書きしない。
    pub fn deny(&self, denial: Denial, now: i64) {
        let mut marks = self.marks();
        // 期限の切れた印を落とす。モデルごとに印が増えるので、掃除しないと
        // 使われなくなったモデル名が溜まり続ける。
        marks.retain(|_, d| d.until > now);

        let denial = match marks.get(&denial.scope) {
            Some(kept) if denial.reason == Reason::Busy && kept.until >= denial.until => {
                kept.clone()
            }
            _ => denial,
        };
        marks.insert(denial.scope.clone(), denial);
        drop(marks);

        // 断られたのも「状態を知った」うち。定期の様子見はここから測る。
        self.ask().heard_at = now;
    }

    /// 印を消す。通った (2xx) 応答に対して呼ぶ。
    ///
    /// このモデルで通ったのだから、**このモデルに効いていた印は全部**間違い
    /// だったことになる。経路全体の印も、モデルの形で括った印も消す。
    ///
    /// Design rationale: 裏で聞きに行った結果と、実際の転送の結果が前後する
    /// のを防ぐ仕掛けは置かない。古い観測で誤って印を外しても、次の実
    /// リクエストが 1 往復で 429 を貰って付け直す。古い観測で誤って印を
    /// 付けても、次に通った応答が消す。どちらも 1 リクエストで直るので、
    /// 順序を守るための仕掛け (世代番号・観測時刻の比較) を足すと、得られる
    /// ものより仕掛けの複雑さの方が大きい。
    pub fn allow(&self, model: &str) {
        self.marks().retain(|scope, _| !scope.covers(model));
    }

    /// この範囲の印だけを消す。枠が塞がっていないと分かったときに呼ぶ。
    pub fn clear(&self, scope: &Scope) {
        self.marks().remove(scope);
    }

    /// このモデルで断られているか。断られていれば、その印。
    ///
    /// このモデルに効く印が複数あるなら**遅く開く方**を返す — この経路が
    /// 使えるようになるのは全部が開いた時なので、早い方を返すとまだ塞がって
    /// いる経路を「もう開く」と伝えることになる。
    pub fn denial(&self, model: &str, now: i64) -> Option<Denial> {
        self.marks()
            .iter()
            .filter(|(scope, _)| scope.covers(model))
            .map(|(_, denial)| denial.clone())
            .filter(|d| d.until > now)
            .max_by_key(|d| d.until)
    }

    /// このモデルを今この経路へ流せるか。
    pub fn availability(&self, model: &str, now: i64) -> Availability {
        match self.denial(model, now) {
            Some(denial) => Availability::Denied {
                until: denial.until,
            },
            None => Availability::Ready,
        }
    }

    /// 枠を聞けたので、印を引き直す。
    ///
    /// 渡すのは provider が枠 1 本ずつを読み解いた結果 —
    /// 「この範囲を、この印で締め出す (`Some`) / 開ける (`None`)」。どの枠が
    /// どの範囲に掛かるかの解釈は provider の知識なので、core は指示された
    /// とおりに印を付け外しするだけ (DR-0014 §3)。
    pub fn apply(&self, entries: &[(Scope, Option<Denial>)], now: i64) {
        for (scope, denial) in entries {
            match denial {
                Some(denial) => self.deny(denial.clone(), now),
                None => self.clear(scope),
            }
        }
    }

    /// 通りすがりに読めた枠を控える。
    pub fn observe_quota(&self, snapshot: Snapshot) {
        *self.quota.lock().unwrap_or_else(PoisonError::into_inner) = Some(snapshot);
    }

    /// 最後に観測した枠。まだ観測していなければ `None`。
    pub fn quota(&self) -> Option<Snapshot> {
        self.quota
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 定期の様子見の役を引き受ける。引き受けられたら札を返す。
    ///
    /// 引き受けるのは、上限で締め出している経路だけ。混雑はいつ空くか
    /// 分からないので短い退避で足り、定期的に聞いても実費が増えるだけになる。
    pub fn claim_probe(&self, now: i64) -> Option<Probing> {
        let limited = self
            .marks()
            .values()
            .any(|d| d.reason == Reason::Limited && d.until > now);
        if !limited {
            return None;
        }
        // 直前に断られたばかりなら、その応答が最新の観測。すぐ聞き直さない。
        if now - self.ask().heard_at < PROBE_INTERVAL {
            return None;
        }
        self.claim(now, PROBE_INTERVAL)
    }

    /// 間隔を待たずに引き受ける。
    ///
    /// どの経路も断られたとき、または窓を伴わない 429 を受けたときに使う。
    /// どちらも「今の枠を知りたい」瞬間で、次の周期を待つ理由がない。
    ///
    /// それでも [`DEFAULT_BACKOFF`] だけは空ける。窓を伴わない断りはその間
    /// どうせ締め出されているので、続けて聞いても結果を使う先がない。
    pub fn claim_ask(&self, now: i64) -> Option<Probing> {
        self.claim(now, DEFAULT_BACKOFF)
    }

    fn claim(&self, now: i64, interval: i64) -> Option<Probing> {
        let mut ask = self.ask();
        if ask.in_flight || (ask.asked_at != 0 && now - ask.asked_at < interval) {
            return None;
        }
        ask.in_flight = true;
        ask.asked_at = now;
        ask.heard_at = now;
        drop(ask);
        Some(Probing {
            ask: Arc::clone(&self.ask),
        })
    }
}

/// 様子を聞いている間の札。持つ者を 1 人に保つ。
///
/// 札を外すのを [`Drop`] に寄せるのは、聞きに行った仕事が途中で落ちても
/// 同じ道を通すため。走り切った経路だけで外すと、落ちたときに札が残り、
/// その経路には二度と聞きに行けなくなる ([`crate::credential`] の更新と同じ形)。
pub struct Probing {
    ask: Arc<Mutex<Ask>>,
}

impl Drop for Probing {
    fn drop(&mut self) {
        self.ask
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    /// 試験で使うモデル名。実際の運用と同じく、断られ方がモデルで違う。
    const FABLE: &str = "claude-fable-5";
    const HAIKU: &str = "claude-haiku-4-5-20251001";

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

    /// あるモデルで断られても、同じ経路の他のモデルは使える。
    ///
    /// これを分けないと、fable の 429 だけで haiku まで止まる。
    #[test]
    fn one_model_denial_leaves_the_others_alone() {
        let state = RouteState::new();
        state.deny(busy(NOW + 60, FABLE), NOW);
        assert_eq!(state.denial(FABLE, NOW), Some(busy(NOW + 60, FABLE)));
        assert_eq!(state.denial(HAIKU, NOW), None);
    }

    /// 窓が塞がっているなら、どのモデルでも使えない。
    #[test]
    fn a_closed_window_stops_every_model() {
        let state = RouteState::new();
        state.deny(limited(NOW + 5000), NOW);
        for model in [FABLE, HAIKU] {
            assert_eq!(
                state.denial(model, NOW),
                Some(limited(NOW + 5000)),
                "{model}"
            );
        }
    }

    #[test]
    fn a_mark_expires() {
        let state = RouteState::new();
        state.deny(limited(NOW + 10), NOW);
        assert_eq!(state.denial(FABLE, NOW), Some(limited(NOW + 10)));
        assert_eq!(state.denial(FABLE, NOW + 10), None, "the deadline is exclusive");
    }

    /// 使えるかどうかは、印の中身を配らずに答える。
    #[test]
    fn availability_answers_without_handing_out_the_mark() {
        let state = RouteState::new();
        assert_eq!(state.availability(FABLE, NOW), Availability::Ready);

        state.deny(limited(NOW + 5000), NOW);
        assert_eq!(
            state.availability(FABLE, NOW),
            Availability::Denied { until: NOW + 5000 }
        );
        assert_eq!(
            state.availability(FABLE, NOW + 5000),
            Availability::Ready,
            "retried once the deadline passes"
        );
    }

    /// 短い退避で、読めていた開く時刻を潰さない。
    #[test]
    fn congestion_does_not_shorten_a_known_deadline() {
        let state = RouteState::new();
        state.deny(limited(NOW + 5000), NOW);
        state.deny(busy(NOW + 60, FABLE), NOW);
        assert_eq!(
            state.denial(HAIKU, NOW),
            Some(limited(NOW + 5000)),
            "a mark for a different scope, so the window deadline is unchanged"
        );
    }

    /// 同じ範囲への短い退避は、期限を縮めない。
    #[test]
    fn a_shorter_backoff_does_not_replace_a_longer_one() {
        let state = RouteState::new();
        state.deny(busy(NOW + 300, FABLE), NOW);
        state.deny(busy(NOW + 60, FABLE), NOW);
        assert_eq!(state.denial(FABLE, NOW), Some(busy(NOW + 300, FABLE)));
    }

    /// 上限のヘッダは正本。前より近い時刻でも、そのまま置く。
    #[test]
    fn a_fresh_limit_replaces_the_deadline() {
        let state = RouteState::new();
        state.deny(limited(NOW + 5000), NOW);
        state.deny(limited(NOW + 100), NOW);
        assert_eq!(state.denial(FABLE, NOW), Some(limited(NOW + 100)));
    }

    /// このモデルで通ったなら、そのモデルの印も経路全体の印も外す。
    #[test]
    fn success_clears_both_marks() {
        let state = RouteState::new();
        state.deny(limited(NOW + 5000), NOW);
        state.deny(busy(NOW + 60, FABLE), NOW);
        state.allow(FABLE);
        assert_eq!(state.denial(FABLE, NOW), None);
        assert_eq!(state.denial(HAIKU, NOW), None);
    }

    /// 別のモデルで通っても、断られているモデルの印までは外さない。
    #[test]
    fn success_with_another_model_keeps_the_model_mark() {
        let state = RouteState::new();
        state.deny(busy(NOW + 60, FABLE), NOW);
        state.allow(HAIKU);
        assert_eq!(state.denial(FABLE, NOW), Some(busy(NOW + 60, FABLE)));
    }

    /// 通ったモデルに効いていた印は、形で括った分も含めて全部消す。
    #[test]
    fn success_clears_the_pattern_that_covered_the_model() {
        let state = RouteState::new();
        state.deny(
            Denial {
                until: NOW + 5000,
                reason: Reason::Limited,
                scope: Scope::Models("claude-fable-*".to_owned()),
            },
            NOW,
        );
        state.allow(FABLE);
        assert_eq!(state.denial(FABLE, NOW), None);
    }

    /// 印が 2 つ生きているなら、この経路が使えるのは遅い方が開いた時。
    #[test]
    fn the_later_of_two_marks_decides_when_the_route_returns() {
        let state = RouteState::new();
        state.deny(limited(NOW + 100), NOW);
        state.deny(busy(NOW + 5000, FABLE), NOW);
        assert_eq!(
            state.denial(FABLE, NOW),
            Some(busy(NOW + 5000, FABLE)),
            "even if the window opens first, a remaining model mark still blocks use"
        );
        assert_eq!(
            state.denial(HAIKU, NOW),
            Some(limited(NOW + 100)),
            "without a mark for that model, only the window is checked"
        );
    }

    /// 期限の切れた印は溜めない。モデル名ごとに印が増えるので、放っておくと
    /// 使われなくなったモデルの分が残り続ける。
    #[test]
    fn expired_marks_are_swept() {
        let state = RouteState::new();
        for model in ["m1", "m2", "m3"] {
            state.deny(busy(NOW + 60, model), NOW);
        }
        assert_eq!(state.marks().len(), 3);

        let later = NOW + 61;
        state.deny(busy(later + 60, "m4"), later);
        assert_eq!(state.marks().len(), 1, "only the one just set is alive");
    }

    /// 枠を聞いた結果は、範囲ごとに印を付けたり外したりする。
    #[test]
    fn applying_quota_results_opens_and_closes_scopes() {
        let state = RouteState::new();
        let fable = Scope::Models("claude-fable-*".to_owned());
        state.apply(
            &[
                (Scope::Everything, Some(limited(NOW + 5000))),
                (
                    fable.clone(),
                    Some(Denial {
                        until: NOW + 5000,
                        reason: Reason::Limited,
                        scope: fable.clone(),
                    }),
                ),
            ],
            NOW,
        );
        assert!(state.denial(FABLE, NOW).is_some());
        assert!(state.denial(HAIKU, NOW).is_some());

        state.apply(&[(Scope::Everything, None)], NOW);
        assert_eq!(state.denial(HAIKU, NOW), None, "the overall quota opened");
        assert!(
            state.denial(FABLE, NOW).is_some(),
            "an unmentioned scope is untouched"
        );
    }

    /// 通りすがりに読めた枠は、経路が持つ。
    #[test]
    fn the_route_keeps_the_quota_it_saw() {
        let state = RouteState::new();
        assert_eq!(state.quota(), None, "nothing before observation");

        let snapshot = Snapshot::new(
            NOW,
            Some(crate::quota::Window {
                utilization: Some(0.5),
                ..Default::default()
            }),
            None,
            None,
        )
        .expect("something was read");
        state.observe_quota(snapshot.clone());
        assert_eq!(state.quota(), Some(snapshot));
    }

    /// 様子を聞く役は 1 人だけ。札は必ず外れる。
    #[test]
    fn only_one_prober_at_a_time() {
        let state = RouteState::new();
        state.deny(limited(NOW + 100_000), NOW);

        let later = NOW + PROBE_INTERVAL;
        let held = state
            .claim_probe(later)
            .expect("claims when the interval has elapsed");
        assert!(
            state.claim_probe(later).is_none(),
            "no one interrupts while a probe is in flight"
        );
        drop(held);
        assert!(
            state.claim_probe(later + PROBE_INTERVAL).is_some(),
            "once the mark is released, it can be claimed again next cycle"
        );
    }

    /// 間隔が空くまでは聞きに行かない。モデル別の短い退避には聞きに行かない。
    #[test]
    fn probing_waits_for_the_interval_and_skips_congestion() {
        let limited_route = RouteState::new();
        limited_route.deny(limited(NOW + 100_000), NOW);
        assert!(
            limited_route
                .claim_probe(NOW + PROBE_INTERVAL - 1)
                .is_none()
        );
        assert!(limited_route.claim_probe(NOW + PROBE_INTERVAL).is_some());

        let busy_route = RouteState::new();
        busy_route.deny(busy(NOW + 100_000, FABLE), NOW);
        assert!(
            busy_route.claim_probe(NOW + PROBE_INTERVAL).is_none(),
            "polling a target with an unknown reopen time only adds real cost"
        );
    }

    /// 印が無い経路にも、急ぎなら聞きに行ける。
    ///
    /// 窓を伴わない 429 を受けた直後がこれ。印は当て推量の 60 秒しか無く、
    /// **その中身を確かめるために聞く**。
    #[test]
    fn an_urgent_ask_does_not_need_a_mark() {
        let state = RouteState::new();
        let held = state.claim_ask(NOW).expect("claims even without a mark");
        assert!(
            state.claim_ask(NOW).is_none(),
            "no one interrupts while a probe is in flight"
        );
        drop(held);
    }

    /// 急ぎでも、続けざまには聞かない。
    ///
    /// 窓を伴わない断りはその間どうせ締め出されているので、続けて聞いても
    /// 結果を使う先がない。
    #[test]
    fn an_urgent_ask_still_leaves_a_gap() {
        let state = RouteState::new();
        drop(state.claim_ask(NOW).expect("first time"));
        assert!(state.claim_ask(NOW + DEFAULT_BACKOFF - 1).is_none());
        assert!(state.claim_ask(NOW + DEFAULT_BACKOFF).is_some());
    }

    #[test]
    fn an_unmarked_route_is_not_probed_on_a_schedule() {
        assert!(RouteState::new().claim_probe(NOW).is_none());
    }
}
