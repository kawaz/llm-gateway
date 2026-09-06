//! provider が抽出し、core が集計する metering の正規形。
//!
//! トークン区分は provider の追加で増えるため閉じた enum にしない。既知の区分は
//! constructor で綴りを揃え、未知の区分も同じ map に失わず保持する。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 拡張可能なトークン区分。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenKind(String);

impl TokenKind {
    pub const INPUT_NAME: &'static str = "input";
    pub const OUTPUT_NAME: &'static str = "output";
    pub const INPUT_CACHE_CREATION_NAME: &'static str = "input.cache_creation";
    /// キャッシュ書き込みのうち、寿命の長い方 (1 時間)。
    ///
    /// TTL 別の単価差を持つ upstream があるので、綴りを core で揃える。値は
    /// [`Self::INPUT_CACHE_CREATION_NAME`] の内数。
    pub const INPUT_CACHE_CREATION_1H_NAME: &'static str = "input.cache_creation.ephemeral_1h";
    /// キャッシュ書き込みのうち、寿命の短い方 (5 分)。同じく親の内数。
    pub const INPUT_CACHE_CREATION_5M_NAME: &'static str = "input.cache_creation.ephemeral_5m";
    pub const INPUT_CACHE_READ_NAME: &'static str = "input.cache_read";
    pub const OUTPUT_REASONING_NAME: &'static str = "output.reasoning";

    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn input() -> Self {
        Self::new(Self::INPUT_NAME)
    }

    pub fn output() -> Self {
        Self::new(Self::OUTPUT_NAME)
    }

    pub fn input_cache_creation() -> Self {
        Self::new(Self::INPUT_CACHE_CREATION_NAME)
    }

    pub fn input_cache_creation_1h() -> Self {
        Self::new(Self::INPUT_CACHE_CREATION_1H_NAME)
    }

    pub fn input_cache_creation_5m() -> Self {
        Self::new(Self::INPUT_CACHE_CREATION_5M_NAME)
    }

    pub fn input_cache_read() -> Self {
        Self::new(Self::INPUT_CACHE_READ_NAME)
    }

    pub fn output_reasoning() -> Self {
        Self::new(Self::OUTPUT_REASONING_NAME)
    }
}

impl From<&str> for TokenKind {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for TokenKind {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for TokenKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 1 応答から抽出したトークン数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub tokens: BTreeMap<TokenKind, u64>,
}

impl TokenUsage {
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    pub fn get(&self, kind: &TokenKind) -> Option<u64> {
        self.tokens.get(kind).copied()
    }

    /// 累積値を取り込む。後から届いた観測が同じ区分を上書きする。
    pub fn set(&mut self, kind: impl Into<TokenKind>, count: u64) {
        self.tokens.insert(kind.into(), count);
    }

    /// 別の応答分を集計へ足す。
    pub fn add_assign(&mut self, other: &Self) {
        for (kind, count) in &other.tokens {
            *self.tokens.entry(kind.clone()).or_default() += count;
        }
    }
}

/// トークン区分ごとの USD / 100 万トークン。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub rates: BTreeMap<TokenKind, f64>,
    /// 内訳区分 → その値を含んでいる親区分。
    ///
    /// 単価の違う内訳を親と一緒に課金するための宣言。内訳が届く記録では
    /// 内訳がそれぞれの単価で、届かない記録では親が全量を負担する
    /// ([`Self::billable`])。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refines: BTreeMap<TokenKind, TokenKind>,
}

impl Pricing {
    /// 単価を持つ区分だけを課金する。
    ///
    /// usage に provider 固有の内訳や親区分の subset が同居していても、単価表へ
    /// 明示していない区分は合計へ入らない。重複しない課金軸の選択は provider の
    /// Metering がこの map を組み立てる時点で行う。
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
        let usd = self.rates.iter().fold(0.0, |total, (kind, rate)| {
            total + self.billable(usage, kind) as f64 * rate / 1_000_000.0
        });
        round_usd(usd)
    }

    /// この区分に何トークン課金するか。
    ///
    /// 親区分は、**単価を持つ内訳の分を引いた残り**だけを負担する。内訳の単価が
    /// 親と違う (キャッシュ書き込みの寿命別の値付け等) 場合に、内訳を別立てで
    /// 課金しても親と二重に数えないため。
    ///
    /// 内訳の届かない記録 — 内訳を返さない provider、内訳を持たない過去日
    /// (DR-0011: 記録はトークン数だけで、USD は閲覧のたびに換算する) — では
    /// 引く相手がいないので、親が全量を負担する = 従来どおりの計算になる。
    ///
    /// 単価を書いていない内訳は引かない。引いてしまうと、その分がどの区分でも
    /// 課金されずに消える。
    fn billable(&self, usage: &TokenUsage, kind: &TokenKind) -> u64 {
        let detailed: u64 = self
            .refines
            .iter()
            .filter(|(child, parent)| *parent == kind && self.rates.contains_key(*child))
            .map(|(child, _)| usage.get(child).unwrap_or(0))
            .sum();
        // 内訳の合計が親を超えていても負にしない (観測値は upstream 任せ)。
        usage.get(kind).unwrap_or(0).saturating_sub(detailed)
    }
}

/// 応答本文を変更せず、通過した chunk から usage を抽出する。
pub trait UsageObserver: Send {
    fn observe(&mut self, chunk: &[u8]);
    fn finish(self: Box<Self>) -> Option<TokenUsage>;
}

/// 集計の 1 行 (credential × モデル) に当てる単価を答える役。
///
/// 記録に残るのはトークン数だけで、USD は**読み出しのたびに**換算する
/// (DR-0011)。その換算に要る単価を持っているのは、答えた経路の provider
/// (DR-0014 §4)。集計の器 ([`crate::stats::Stats`]) は単価表を持たず、
/// この役へ聞く。
///
/// 鍵に credential を含めるのは、同じモデル名でも経路によって値付けが違い
/// うるため。認証情報を持たない経路 ([`crate::stats::NO_CREDENTIAL`]) や
/// 設定から消えた名前も引かれるので、実装側は「この名前は知らない」を
/// `None` ではなくモデル名からの解決で埋めてよい。
pub trait PricingSource {
    fn pricing(&self, credential: &str, model: &str) -> Option<Pricing>;
}

/// 合計を足し込むときも同じ丸めを通す。
pub fn round_usd(usd: f64) -> f64 {
    (usd * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 標準区分の綴りを固定しつつ、provider 固有区分も同じ形で保持できる。
    #[test]
    fn token_kinds_are_open_ended() {
        let mut usage = TokenUsage::default();
        usage.set(TokenKind::input(), 10);
        usage.set(TokenKind::output_reasoning(), 4);
        usage.set("provider.batch_prediction", 3);

        assert_eq!(usage.get(&TokenKind::input()), Some(10));
        assert_eq!(usage.get(&TokenKind::output_reasoning()), Some(4));
        assert_eq!(
            usage.get(&TokenKind::new("provider.batch_prediction")),
            Some(3),
            "unknown categories are not collapsed into other"
        );
    }

    /// Pricing に無い detail は保存しても課金へ重ねない。
    #[test]
    fn unpriced_subset_details_are_not_double_charged() {
        let usage = TokenUsage {
            tokens: BTreeMap::from([
                (TokenKind::input(), 1_000_000),
                (TokenKind::new("input.cached_detail"), 900_000),
            ]),
        };
        let pricing = Pricing {
            rates: BTreeMap::from([(TokenKind::input(), 5.0)]),
            ..Pricing::default()
        };

        assert_eq!(pricing.cost(&usage), 5.0);
        assert_eq!(
            usage.get(&TokenKind::new("input.cached_detail")),
            Some(900_000),
            "non-billed breakdowns are still kept as observed values"
        );
    }

    /// 単価を定義した独立区分はそれぞれ合計へ入る。
    #[test]
    fn every_priced_kind_contributes() {
        let usage = TokenUsage {
            tokens: BTreeMap::from([
                (TokenKind::input(), 1_000_000),
                (TokenKind::output(), 1_000_000),
                (TokenKind::input_cache_read(), 1_000_000),
            ]),
        };
        let pricing = Pricing {
            rates: BTreeMap::from([
                (TokenKind::input(), 5.0),
                (TokenKind::output(), 25.0),
                (TokenKind::input_cache_read(), 0.5),
            ]),
            ..Pricing::default()
        };

        assert_eq!(pricing.cost(&usage), 30.5);
    }

    /// 内訳を別単価で課金する表。親は内訳の残りだけを負担する。
    fn split_cache_write() -> Pricing {
        Pricing {
            rates: BTreeMap::from([
                (TokenKind::input_cache_creation(), 1.25),
                (TokenKind::input_cache_creation_1h(), 2.0),
            ]),
            refines: BTreeMap::from([(
                TokenKind::input_cache_creation_1h(),
                TokenKind::input_cache_creation(),
            )]),
        }
    }

    /// 内訳が届いた分はその単価で、残りは親の単価で課金する。
    #[test]
    fn a_priced_breakdown_replaces_its_share_of_the_parent() {
        let usage = TokenUsage {
            tokens: BTreeMap::from([
                (TokenKind::input_cache_creation(), 1_000_000),
                (TokenKind::input_cache_creation_1h(), 600_000),
                (TokenKind::input_cache_creation_5m(), 400_000),
            ]),
        };

        // 1h 60 万 x $2 + 残り 40 万 x $1.25 = $1.2 + $0.5。
        assert_eq!(split_cache_write().cost(&usage), 1.7);
    }

    /// 内訳の無い記録では、親が全量を負担する。
    ///
    /// 過去日や、内訳を返さない provider の記録が従来どおりに計算される。
    #[test]
    fn a_parent_without_a_breakdown_is_charged_in_full() {
        let usage = TokenUsage {
            tokens: BTreeMap::from([(TokenKind::input_cache_creation(), 1_000_000)]),
        };

        assert_eq!(split_cache_write().cost(&usage), 1.25);
    }

    /// 内訳の合計が親を超えていても、負の課金にはしない。
    #[test]
    fn an_oversized_breakdown_does_not_go_negative() {
        let usage = TokenUsage {
            tokens: BTreeMap::from([
                (TokenKind::input_cache_creation(), 100),
                (TokenKind::input_cache_creation_1h(), 1_000_000),
            ]),
        };

        // 親の取り分は 0 まで。1h の 100 万 x $2 だけが残る。
        assert_eq!(split_cache_write().cost(&usage), 2.0);
    }

    /// 単価を書いていない内訳は親から引かない。
    ///
    /// 引くと、その分がどの区分でも課金されずに消える。
    #[test]
    fn an_unpriced_breakdown_is_not_subtracted() {
        let usage = TokenUsage {
            tokens: BTreeMap::from([
                (TokenKind::input_cache_creation(), 1_000_000),
                (TokenKind::input_cache_creation_5m(), 1_000_000),
            ]),
        };
        // 5m は親と同じ単価なので単価表に持たない = 親が全量を負担する。
        assert_eq!(split_cache_write().cost(&usage), 1.25);
    }
}
