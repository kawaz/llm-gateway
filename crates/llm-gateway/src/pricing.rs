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
//! # 行が区分の名前を持つ
//!
//! 1 行がそのまま「どの区分にいくら掛かるか」の表 ([`Pricing`]) になる。列を
//! 固定した形にすると、区分の増える provider (reasoning を別立てで課金する等)
//! を足すときに表の形から直すことになる (DR-0014 §4)。
//!
//! 並べる区分は**互いに重ならないもの**だけにする。合計は行に挙げた区分の
//! 足し算なので、親区分とその内訳を両方書くと二重に課金される。
//!
//! # 倍率でなく実値で持つ
//!
//! Anthropic の cache write (5 分) は input の 1.25 倍、cache read は 0.1 倍だが、
//! 倍率としてではなく計算済みの実値を書く。倍率をコードに埋めると、倍率の違う
//! upstream (OpenAI 系には cache write の割増が無い) を足したときに
//! 表とコードの両方を直すことになる。

use crate::metering::{Pricing, TokenKind};
use crate::pattern;

// 表を読みやすくするための別名。指しているのは core の区分の綴り。
const INPUT: &str = TokenKind::INPUT_NAME;
const OUTPUT: &str = TokenKind::OUTPUT_NAME;
const CACHE_WRITE: &str = TokenKind::INPUT_CACHE_CREATION_NAME;
const CACHE_READ: &str = TokenKind::INPUT_CACHE_READ_NAME;

/// 単価表の 1 行。`patterns` のどれかに当たれば `rates` を使う。
struct Row {
    patterns: &'static [&'static str],
    /// 課金する区分と、その 100 万トークンあたりの USD。
    rates: &'static [(&'static str, f64)],
}

const fn row(patterns: &'static [&'static str], rates: &'static [(&'static str, f64)]) -> Row {
    Row { patterns, rates }
}

/// 単価表。上から順に見て、最初に当たった行を使う。
///
/// 出典と確認日はモデル群ごとに脚注で示す。数字を直したら確認日も直す。
static TABLE: &[Row] = &[
    // --- Anthropic (2026-07-31 確認 / claude-api skill の Current Models 表)
    //     cache write = input x1.25 (5m TTL), cache read = input x0.1。
    //     input はキャッシュ分を含まないので、4 区分は重ならない。
    row(
        &["claude-fable-5", "claude-fable-5-*"],
        &[
            (INPUT, 10.0),
            (OUTPUT, 50.0),
            (CACHE_WRITE, 12.5),
            (CACHE_READ, 1.0),
        ],
    ),
    row(
        &["claude-mythos-5", "claude-mythos-5-*"],
        &[
            (INPUT, 10.0),
            (OUTPUT, 50.0),
            (CACHE_WRITE, 12.5),
            (CACHE_READ, 1.0),
        ],
    ),
    row(
        &["claude-opus-5", "claude-opus-5-*"],
        &[
            (INPUT, 5.0),
            (OUTPUT, 25.0),
            (CACHE_WRITE, 6.25),
            (CACHE_READ, 0.5),
        ],
    ),
    row(
        &["claude-opus-4-8", "claude-opus-4-8-*"],
        &[
            (INPUT, 5.0),
            (OUTPUT, 25.0),
            (CACHE_WRITE, 6.25),
            (CACHE_READ, 0.5),
        ],
    ),
    row(
        &["claude-opus-4-7", "claude-opus-4-7-*"],
        &[
            (INPUT, 5.0),
            (OUTPUT, 25.0),
            (CACHE_WRITE, 6.25),
            (CACHE_READ, 0.5),
        ],
    ),
    row(
        &["claude-opus-4-6", "claude-opus-4-6-*"],
        &[
            (INPUT, 5.0),
            (OUTPUT, 25.0),
            (CACHE_WRITE, 6.25),
            (CACHE_READ, 0.5),
        ],
    ),
    // sonnet-5 は 2026-08-31 まで導入価格 ($2/$10) が案内されている。表には
    // 通常価格を置く — 導入価格が自分の契約に効いているか確認できておらず、
    // 期限切れを取りこぼすと黙って安く見積もり続けるため。
    row(
        &["claude-sonnet-5", "claude-sonnet-5-*"],
        &[
            (INPUT, 3.0),
            (OUTPUT, 15.0),
            (CACHE_WRITE, 3.75),
            (CACHE_READ, 0.3),
        ],
    ),
    row(
        &["claude-sonnet-4-6", "claude-sonnet-4-6-*"],
        &[
            (INPUT, 3.0),
            (OUTPUT, 15.0),
            (CACHE_WRITE, 3.75),
            (CACHE_READ, 0.3),
        ],
    ),
    row(
        &["claude-haiku-4-5", "claude-haiku-4-5-*"],
        &[
            (INPUT, 1.0),
            (OUTPUT, 5.0),
            (CACHE_WRITE, 1.25),
            (CACHE_READ, 0.1),
        ],
    ),
    // --- OpenAI gpt-5.6 系 (2026-07-31 確認 / developers.openai.com/api/docs/pricing)
    //     Standard tier・短コンテキストの値。cache write の割増は無いので
    //     input と同額、cache read は cached input の値。長コンテキスト
    //     (272K 超入力) は別料金だが、応答からは判別できないので採らない。
    //     reasoning は output に含めて請求されるので、別区分では課金しない。
    row(
        &["gpt-5.6-sol", "gpt-5.6-sol-*"],
        &[
            (INPUT, 5.0),
            (OUTPUT, 30.0),
            (CACHE_WRITE, 5.0),
            (CACHE_READ, 0.5),
        ],
    ),
    row(
        &["gpt-5.6-terra", "gpt-5.6-terra-*"],
        &[
            (INPUT, 2.0),
            (OUTPUT, 12.0),
            (CACHE_WRITE, 2.0),
            (CACHE_READ, 0.2),
        ],
    ),
    row(
        &["gpt-5.6-luna", "gpt-5.6-luna-*"],
        &[
            (INPUT, 0.2),
            (OUTPUT, 1.2),
            (CACHE_WRITE, 0.2),
            (CACHE_READ, 0.02),
        ],
    ),
];

/// モデル名に対応する単価。表に無ければ `None`。
pub fn for_model(model: &str) -> Option<Pricing> {
    let row = TABLE
        .iter()
        .find(|row| pattern::matches_any(row.patterns, model))?;
    Some(Pricing {
        rates: row
            .rates
            .iter()
            .map(|(kind, usd)| (TokenKind::new(*kind), *usd))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metering::TokenUsage;

    /// その区分の単価。表に載っていない区分は `None`。
    fn rate(model: &str, kind: &TokenKind) -> Option<f64> {
        for_model(model)?.rates.get(kind).copied()
    }

    /// 1 区分だけ使った usage。
    fn usage(kind: TokenKind, count: u64) -> TokenUsage {
        let mut usage = TokenUsage::default();
        usage.set(kind, count);
        usage
    }

    /// 呼び名がそのまま当たる。
    #[test]
    fn a_plain_name_matches() {
        assert_eq!(rate("claude-opus-5", &TokenKind::input()), Some(5.0));
        assert_eq!(rate("claude-fable-5", &TokenKind::output()), Some(50.0));
        assert_eq!(rate("gpt-5.6-luna", &TokenKind::output()), Some(1.2));
    }

    /// 日付付きの形も同じ行に当たる。
    #[test]
    fn a_dated_name_matches_the_same_row() {
        let plain = for_model("claude-haiku-4-5").unwrap();
        let dated = for_model("claude-haiku-4-5-20251001").unwrap();
        assert_eq!(plain, dated);
    }

    /// 前方一致で隣のモデルを巻き込まない。
    #[test]
    fn a_near_miss_does_not_match() {
        // `claude-opus-5*` と書いていたら 50 も拾ってしまう。
        assert!(for_model("claude-opus-50").is_none());
        assert_ne!(
            for_model("claude-opus-5").unwrap(),
            for_model("claude-haiku-4-5").unwrap()
        );
    }

    /// 知らないモデルは `None`。推測した単価を返さない。
    #[test]
    fn an_unknown_model_has_no_rates() {
        assert!(for_model("some-other-model").is_none());
        assert!(for_model("").is_none());
        // 予約名 (認証情報を持たない経路) もモデルではないので当たらない。
        assert!(for_model(crate::stats::NO_CREDENTIAL).is_none());
    }

    /// 表に挙げた区分がそれぞれ掛かる。
    #[test]
    fn every_listed_kind_is_applied() {
        let p = for_model("claude-opus-5").unwrap();
        // input 100 万 = $5、output 100 万 = $25、
        // cache write 100 万 = $6.25、cache read 100 万 = $0.5。
        assert_eq!(p.cost(&usage(TokenKind::input(), 1_000_000)), 5.0);
        assert_eq!(p.cost(&usage(TokenKind::output(), 1_000_000)), 25.0);
        assert_eq!(
            p.cost(&usage(TokenKind::input_cache_creation(), 1_000_000)),
            6.25
        );
        assert_eq!(
            p.cost(&usage(TokenKind::input_cache_read(), 1_000_000)),
            0.5
        );

        let mut all = TokenUsage::default();
        for kind in [
            TokenKind::input(),
            TokenKind::output(),
            TokenKind::input_cache_creation(),
            TokenKind::input_cache_read(),
        ] {
            all.set(kind, 1_000_000);
        }
        assert_eq!(p.cost(&all), 36.75);
    }

    /// 表に無い区分は課金に入らない。
    ///
    /// provider が固有の内訳を残しても、単価を書いていない限り合計は動かない。
    #[test]
    fn an_unlisted_kind_costs_nothing() {
        let p = for_model("claude-opus-5").unwrap();
        assert_eq!(
            p.cost(&usage(TokenKind::new("input.audio"), 1_000_000)),
            0.0
        );
        assert_eq!(
            p.cost(&usage(TokenKind::output_reasoning(), 1_000_000)),
            0.0,
            "reasoning は output に含めて請求されるので別立てにしない"
        );
    }

    /// 0 トークンは 0 ドル (`None` ではない)。使っていない行も欄は出る。
    #[test]
    fn zero_tokens_cost_zero() {
        assert_eq!(
            for_model("claude-opus-5")
                .unwrap()
                .cost(&TokenUsage::default()),
            0.0
        );
    }

    /// 端数は小数第 6 位で切り揃える。浮動小数の末尾で試験が揺れないように。
    #[test]
    fn the_result_is_rounded() {
        let p = for_model("claude-haiku-4-5").unwrap();
        // 1 トークンは $0.000001。
        assert_eq!(p.cost(&usage(TokenKind::input(), 1)), 0.000_001);
        // 丸めの下に落ちる分は 0 になる (cache read 1 トークン = $0.0000001)。
        assert_eq!(p.cost(&usage(TokenKind::input_cache_read(), 1)), 0.0);
    }
}
