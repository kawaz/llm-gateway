//! 枠照会 API を定期的に聞く順番 (DR-0044 決定 9)。
//!
//! モデル別の枠 (Anthropic の `weekly_scoped` など) は応答ヘッダに出ず、枠照会
//! API ([`crate::provider::QuotaApi`]) でしか取れない。聞く契機が手動の
//! `usage?refresh=true` や締め出しの周りだけだと、推移 ([`super::history`]) の
//! モデル別の枠の行がまばらになる。そこで周期 (既定 5 分) ごとに聞く。
//!
//! ## 聞く相手
//!
//! **前回この周期で聞けてから、このプロセスがその credential で 1 本でも流した
//! もの**だけ。流した本数は日次集計の器が起動から積んだ累計
//! ([`crate::stats::Stats::running_totals`] の `requests` の和) で見る。減らない
//! 数なので、聞けた時の値より増えていれば「その後に流した」と言える。
//!
//! - 流していない credential は聞かない。枠は動いていない (動いたとしても
//!   gateway の外の利用で、推移の行に添えるトークンが無い)
//! - 起動直後は累計が 0 なので、最初の 1 本を流すまで聞かない
//! - 他の契機 (`usage?refresh=true` など) で聞いた分は数えない。重なっても
//!   トークンを使わない照会が 1 回増えるだけで、契機ごとの記録を突き合わせる
//!   仕掛けの方が重い
//!
//! ## 聞けなかった時
//!
//! best-effort。印を据え置き、本数の控えも動かさないので、次の周期でまた
//! 聞く。続けて失敗する相手には周期を空けていく (2 回目の失敗の後は 2 周期、
//! 3 回目は 4 周期 …、[`BACKOFF_CAP`] まで)。429 (聞きすぎ) は 1 回目から
//! [`BACKOFF_CAP`] まで空ける — 同じ間隔で聞き続ける理由が無い。
//!
//! 周期は時刻でなく回数で数える。周期ちょうどに時刻で比べると、秒の丸めで
//! 1 周期ぶん飛ぶことがある。

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// 失敗が続いた時に空ける長さの上限。
///
/// 長くすると、相手が直った後も推移の行が欠ける時間が延びる。1 時間なら、
/// 7 日の枠の 1 % (全力で使って 1〜2 時間) を取りこぼすことはほぼ無く、
/// 聞きすぎと言われた相手にも 1 時間に 1 回しか当たらない。
pub const BACKOFF_CAP: Duration = Duration::from_secs(60 * 60);

/// credential ごとに覚えておくこと。
#[derive(Default)]
struct Mark {
    /// 最後に聞けた時点の、流した本数の累計。
    answered_at: u64,
    /// 続けて失敗した回数。聞けたら 0 に戻る。
    failures: u32,
    /// あと何周期見送るか。
    skip: u32,
}

/// 周期ごとに「どの credential に聞くか」を決める。
pub struct Schedule {
    /// [`BACKOFF_CAP`] が何周期ぶんか (切り上げ、1 以上)。
    cap_periods: u32,
    marks: Mutex<BTreeMap<String, Mark>>,
}

impl Schedule {
    /// 周期の長さから組む。
    pub fn new(every: Duration) -> Self {
        let every = every.as_secs().max(1);
        let cap_periods = BACKOFF_CAP.as_secs().div_ceil(every).max(1);
        Self {
            cap_periods: u32::try_from(cap_periods).unwrap_or(u32::MAX),
            marks: Mutex::new(BTreeMap::new()),
        }
    }

    fn marks(&self) -> MutexGuard<'_, BTreeMap<String, Mark>> {
        self.marks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// この周期にこの credential へ聞くか。1 周期に 1 回、各 credential について
    /// 呼ぶ (見送りの残りをここで 1 つ減らす)。
    ///
    /// `requests` はこのプロセスがその credential で流した本数の、起動からの累計。
    pub fn due(&self, credential: &str, requests: u64) -> bool {
        let mut marks = self.marks();
        match marks.get_mut(credential) {
            Some(mark) if mark.skip > 0 => {
                mark.skip -= 1;
                false
            }
            Some(mark) => requests > mark.answered_at,
            None => requests > 0,
        }
    }

    /// 聞けた。`requests` は [`Self::due`] に渡したのと同じ値 (聞いている間に
    /// 流した分は、次の周期で数える)。
    pub fn answered(&self, credential: &str, requests: u64) {
        let mut marks = self.marks();
        let mark = marks.entry(credential.to_owned()).or_default();
        mark.answered_at = requests;
        mark.failures = 0;
        mark.skip = 0;
    }

    /// 聞けなかった。`rate_limited` は相手が 429 を返したか。
    pub fn failed(&self, credential: &str, rate_limited: bool) {
        let mut marks = self.marks();
        let mark = marks.entry(credential.to_owned()).or_default();
        mark.failures = mark.failures.saturating_add(1);
        let periods = if rate_limited {
            self.cap_periods
        } else {
            // 1 回目の失敗は次の周期、以後は倍々。
            2u32.checked_pow(mark.failures - 1)
                .map_or(self.cap_periods, |n| n.min(self.cap_periods))
        };
        mark.skip = periods - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIVE_MINUTES: Duration = Duration::from_secs(5 * 60);

    /// 周期ごとに `due` を呼び、聞くと答えた周期の番号 (1 始まり) を返す。
    /// 聞いた周期では `outcome` を渡す。
    fn run(
        schedule: &Schedule,
        periods: u32,
        requests: u64,
        mut outcome: impl FnMut(&Schedule),
    ) -> Vec<u32> {
        (1..=periods)
            .filter(|_| {
                let due = schedule.due("claude-a", requests);
                if due {
                    outcome(schedule);
                }
                due
            })
            .collect()
    }

    // ---------- 聞く相手: 前回聞けてから流した credential だけ ----------

    /// 起動から 1 本も流していない credential には聞かない。
    ///
    /// 流していなければこのプロセスが添えるトークンが無く、照会はモデル別の
    /// 枠の行を 1 つ増やすだけで比較の材料にならない。使っていない credential
    /// を叩かないのは kawaz 裁定 (2026-11-21) の形そのもの。
    #[test]
    fn an_unused_credential_is_not_asked() {
        let schedule = Schedule::new(FIVE_MINUTES);
        assert!(!schedule.due("claude-a", 0));
        assert!(
            schedule.due("claude-a", 1),
            "the first request makes it due"
        );
    }

    /// 聞けた後は、流した本数が増えるまで聞かない。増えたらすぐ (次の周期で) 聞く。
    ///
    /// 累計は減らない数なので、聞けた時の値と比べれば「その後に流したか」が
    /// 言える。同じ値のままなら枠を動かしたのは gateway の外だけで、聞いても
    /// 行に添えるトークンが無い。
    #[test]
    fn after_an_answer_only_new_traffic_makes_it_due() {
        let schedule = Schedule::new(FIVE_MINUTES);
        assert!(schedule.due("claude-a", 3));
        schedule.answered("claude-a", 3);

        assert!(!schedule.due("claude-a", 3), "nothing flowed since");
        assert!(!schedule.due("claude-a", 3));
        assert!(schedule.due("claude-a", 4), "one more request is enough");
    }

    /// credential ごとに別々に数える。
    #[test]
    fn credentials_are_counted_apart() {
        let schedule = Schedule::new(FIVE_MINUTES);
        schedule.answered("claude-a", 5);
        assert!(!schedule.due("claude-a", 5));
        assert!(schedule.due("claude-b", 5), "b has not been answered yet");
    }

    // ---------- 聞けなかった時 ----------

    /// 1 回目の失敗は次の周期で聞き直す。本数の控えは動かない (流した分を
    /// 聞けていないので、流していなくても聞き直す)。
    #[test]
    fn a_first_failure_is_retried_on_the_next_period() {
        let schedule = Schedule::new(FIVE_MINUTES);
        schedule.answered("claude-a", 3);
        assert!(schedule.due("claude-a", 4));
        schedule.failed("claude-a", false);

        assert!(
            schedule.due("claude-a", 4),
            "the unanswered traffic is still pending"
        );
    }

    /// 失敗が続くと 1, 2, 4, 8 … 周期ごとに聞き、上限 (5 分周期なら 12 周期 =
    /// 1 時間) で止まる。
    ///
    /// 相手が壊れている (形が変わった、認証が切れた) 時に、5 分ごとに警告と
    /// 照会を積み上げない。上限を設けるのは、直った後の取りこぼしを 1 時間で
    /// 抑えるため。
    #[test]
    fn repeated_failures_back_off_up_to_an_hour() {
        let schedule = Schedule::new(FIVE_MINUTES);
        let asked = run(&schedule, 50, 1, |s| s.failed("claude-a", false));
        let gaps: Vec<u32> = asked.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(asked[0], 1);
        assert_eq!(gaps, [1, 2, 4, 8, 12, 12]);
    }

    /// 429 (聞きすぎ) は 1 回目から上限まで空ける。
    ///
    /// 聞きすぎと言われた相手に同じ間隔で聞き直すと、また 429 を貰うだけになる。
    #[test]
    fn a_rate_limit_waits_the_full_hour() {
        let schedule = Schedule::new(FIVE_MINUTES);
        let asked = run(&schedule, 30, 1, |s| s.failed("claude-a", true));
        assert_eq!(asked, [1, 13, 25]);
    }

    /// 聞けたら失敗の数は戻る。その後の失敗は、また次の周期から聞き直す。
    ///
    /// 4 回続けて失敗した相手 (次は 8 周期後) でも、1 度聞けたら過去の失敗を
    /// 引きずらない。引きずると、一時的な不調の後もしばらく行が疎になる。
    #[test]
    fn an_answer_resets_the_backoff() {
        let schedule = Schedule::new(FIVE_MINUTES);
        for _ in 0..4 {
            schedule.failed("claude-a", false);
        }
        schedule.answered("claude-a", 1);

        assert!(schedule.due("claude-a", 2), "the answer lifted the wait");
        schedule.failed("claude-a", false);
        assert!(schedule.due("claude-a", 2), "back to the next period");
    }

    /// 上限の周期数は切り上げ。周期が 1 時間以上なら上限は 1 周期 (次の周期)。
    ///
    /// 7 分周期なら 9 周期 (63 分)。切り捨てると上限が 1 時間を割り、聞きすぎと
    /// 言われた相手に 1 時間より早く当たる。
    #[test]
    fn the_cap_is_rounded_up_to_whole_periods() {
        assert_eq!(Schedule::new(Duration::from_secs(7 * 60)).cap_periods, 9);
        assert_eq!(Schedule::new(FIVE_MINUTES).cap_periods, 12);
        assert_eq!(Schedule::new(Duration::from_secs(60 * 60)).cap_periods, 1);
        assert_eq!(
            Schedule::new(Duration::from_secs(3 * 60 * 60)).cap_periods,
            1
        );

        let hourly = Schedule::new(Duration::from_secs(60 * 60));
        hourly.failed("claude-a", true);
        assert!(hourly.due("claude-a", 1), "an hour has already passed");
    }
}
