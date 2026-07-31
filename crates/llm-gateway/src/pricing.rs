//! モデルごとの単価表 (USD / 100 万トークン)。
//!
//! # なぜソースに埋め込むか
//!
//! 単価は「gateway を動かす人が決める設定」ではなく「upstream が決める事実」。
//! config に置くと、値を書かない限りコストが出ない・人によって違う値が入る、の
//! 両方が起きる。事実はソースに 1 本持ち、改定はこのファイルの変更として残す。
//!
//! # 保守のしかた
//!
//! 単価が改定されたら [`TABLE`] の当該行の数字だけ直す。過去日の記録は
//! トークン数しか持たず、コストは**閲覧のたびに計算する** (DR-0011) ので、
//! 直した値が過去日にも遡って効く。当時の単価で見たいという要求は無い。
//!
//! モデルを足すときは行を 1 つ増やす。`patterns` には呼び名と日付付きの形の
//! 両方を書く (`claude-haiku-4-5` と `claude-haiku-4-5-20251001` は同じ単価)。
//! **表に無いモデルはコストを出さない** — 推測した数字を出すくらいなら、
//! 欄ごと無い方がよい。
//!
//! # 4 つのレートは実値で持つ
//!
//! Anthropic の cache write (5 分) は input の 1.25 倍、cache read は 0.1 倍だが、
//! 倍率としてではなく計算済みの実値を書く。倍率をコードに埋めると、倍率の違う
//! upstream (OpenAI 系には cache write の割増が無い) を足したときに
//! 表とコードの両方を直すことになる。

use crate::pattern;

/// 100 万トークンあたりの USD。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    /// キャッシュへの書き込み。Anthropic は 5 分 TTL 前提の値。
    pub cache_write: f64,
    /// キャッシュからの読み出し。
    pub cache_read: f64,
}

impl Rates {
    /// トークン数から USD を出す。
    ///
    /// 小数第 6 位で丸めるのは、足し込みの順で末尾がぶれないようにするため
    /// (0.1 セントより下は表示にも使わない)。
    pub fn cost(&self, input: u64, output: u64, cache_write: u64, cache_read: u64) -> f64 {
        let per = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
        round_usd(
            per(input, self.input)
                + per(output, self.output)
                + per(cache_write, self.cache_write)
                + per(cache_read, self.cache_read),
        )
    }
}

/// 合計を足し込むときも同じ丸めを通す。
pub fn round_usd(usd: f64) -> f64 {
    (usd * 1_000_000.0).round() / 1_000_000.0
}

/// 単価表の 1 行。`patterns` のどれかに当たれば `rates` を使う。
struct Row {
    patterns: &'static [&'static str],
    rates: Rates,
}

const fn row(patterns: &'static [&'static str], rates: [f64; 4]) -> Row {
    Row {
        patterns,
        rates: Rates {
            input: rates[0],
            output: rates[1],
            cache_write: rates[2],
            cache_read: rates[3],
        },
    }
}

/// 単価表。上から順に見て、最初に当たった行を使う。
///
/// 出典と確認日はモデル群ごとに脚注で示す。数字を直したら確認日も直す。
static TABLE: &[Row] = &[
    // --- Anthropic (2026-07-31 確認 / claude-api skill の Current Models 表)
    //     cache write = input x1.25 (5m TTL), cache read = input x0.1。
    row(
        &["claude-fable-5", "claude-fable-5-*"],
        [10.0, 50.0, 12.5, 1.0],
    ),
    row(
        &["claude-mythos-5", "claude-mythos-5-*"],
        [10.0, 50.0, 12.5, 1.0],
    ),
    row(
        &["claude-opus-5", "claude-opus-5-*"],
        [5.0, 25.0, 6.25, 0.5],
    ),
    row(
        &["claude-opus-4-8", "claude-opus-4-8-*"],
        [5.0, 25.0, 6.25, 0.5],
    ),
    row(
        &["claude-opus-4-7", "claude-opus-4-7-*"],
        [5.0, 25.0, 6.25, 0.5],
    ),
    row(
        &["claude-opus-4-6", "claude-opus-4-6-*"],
        [5.0, 25.0, 6.25, 0.5],
    ),
    // sonnet-5 は 2026-08-31 まで導入価格 ($2/$10) が案内されている。表には
    // 通常価格を置く — 導入価格が自分の契約に効いているか確認できておらず、
    // 期限切れを取りこぼすと黙って安く見積もり続けるため。
    row(
        &["claude-sonnet-5", "claude-sonnet-5-*"],
        [3.0, 15.0, 3.75, 0.3],
    ),
    row(
        &["claude-sonnet-4-6", "claude-sonnet-4-6-*"],
        [3.0, 15.0, 3.75, 0.3],
    ),
    row(
        &["claude-haiku-4-5", "claude-haiku-4-5-*"],
        [1.0, 5.0, 1.25, 0.1],
    ),
    // --- OpenAI gpt-5.6 系 (2026-07-31 確認 / developers.openai.com/api/docs/pricing)
    //     Standard tier・短コンテキストの値。cache write の割増は無いので
    //     input と同額、cache read は cached input の値。長コンテキスト
    //     (272K 超入力) は別料金だが、応答からは判別できないので採らない。
    row(&["gpt-5.6-sol", "gpt-5.6-sol-*"], [5.0, 30.0, 5.0, 0.5]),
    row(&["gpt-5.6-terra", "gpt-5.6-terra-*"], [2.0, 12.0, 2.0, 0.2]),
    row(&["gpt-5.6-luna", "gpt-5.6-luna-*"], [0.2, 1.2, 0.2, 0.02]),
];

/// モデル名に対応する単価。表に無ければ `None`。
pub fn rates_for(model: &str) -> Option<Rates> {
    TABLE
        .iter()
        .find(|row| pattern::matches_any(row.patterns, model))
        .map(|row| row.rates)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 呼び名がそのまま当たる。
    #[test]
    fn a_plain_name_matches() {
        assert_eq!(rates_for("claude-opus-5").unwrap().input, 5.0);
        assert_eq!(rates_for("claude-fable-5").unwrap().output, 50.0);
        assert_eq!(rates_for("gpt-5.6-luna").unwrap().output, 1.2);
    }

    /// 日付付きの形も同じ行に当たる。
    #[test]
    fn a_dated_name_matches_the_same_row() {
        let plain = rates_for("claude-haiku-4-5").unwrap();
        let dated = rates_for("claude-haiku-4-5-20251001").unwrap();
        assert_eq!(plain, dated);
    }

    /// 前方一致で隣のモデルを巻き込まない。
    #[test]
    fn a_near_miss_does_not_match() {
        // `claude-opus-5*` と書いていたら 50 も拾ってしまう。
        assert!(rates_for("claude-opus-50").is_none());
        assert_ne!(
            rates_for("claude-opus-5").unwrap(),
            rates_for("claude-haiku-4-5").unwrap()
        );
    }

    /// 知らないモデルは `None`。推測した単価を返さない。
    #[test]
    fn an_unknown_model_has_no_rates() {
        assert!(rates_for("some-other-model").is_none());
        assert!(rates_for("").is_none());
        // 予約名 (認証情報を持たない経路) もモデルではないので当たらない。
        assert!(rates_for(crate::stats::NO_CREDENTIAL).is_none());
    }

    /// 4 つのレートがそれぞれ掛かる。
    #[test]
    fn every_rate_is_applied() {
        let r = rates_for("claude-opus-5").unwrap();
        // input 100 万 = $5、output 100 万 = $25、
        // cache write 100 万 = $6.25、cache read 100 万 = $0.5。
        assert_eq!(r.cost(1_000_000, 0, 0, 0), 5.0);
        assert_eq!(r.cost(0, 1_000_000, 0, 0), 25.0);
        assert_eq!(r.cost(0, 0, 1_000_000, 0), 6.25);
        assert_eq!(r.cost(0, 0, 0, 1_000_000), 0.5);
        assert_eq!(r.cost(1_000_000, 1_000_000, 1_000_000, 1_000_000), 36.75);
    }

    /// 0 トークンは 0 ドル (`None` ではない)。使っていない行も欄は出る。
    #[test]
    fn zero_tokens_cost_zero() {
        assert_eq!(rates_for("claude-opus-5").unwrap().cost(0, 0, 0, 0), 0.0);
    }

    /// 端数は小数第 6 位で切り揃える。浮動小数の末尾で試験が揺れないように。
    #[test]
    fn the_result_is_rounded() {
        let r = rates_for("claude-haiku-4-5").unwrap();
        // 1 トークンは $0.000001。
        assert_eq!(r.cost(1, 0, 0, 0), 0.000_001);
        // 丸めの下に落ちる分は 0 になる (cache read 1 トークン = $0.0000001)。
        assert_eq!(r.cost(0, 0, 0, 1), 0.0);
    }
}
