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
}

impl Pricing {
    /// 単価を持つ区分だけを課金する。
    ///
    /// usage に provider 固有の内訳や親区分の subset が同居していても、単価表へ
    /// 明示していない区分は合計へ入らない。重複しない課金軸の選択は provider の
    /// Metering がこの map を組み立てる時点で行う。
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
        let usd = self.rates.iter().fold(0.0, |total, (kind, rate)| {
            total + usage.get(kind).unwrap_or(0) as f64 * rate / 1_000_000.0
        });
        round_usd(usd)
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
            "未知の区分も other へ潰さない"
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
        };

        assert_eq!(pricing.cost(&usage), 5.0);
        assert_eq!(
            usage.get(&TokenKind::new("input.cached_detail")),
            Some(900_000),
            "課金しない内訳も観測値としては保持する"
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
        };

        assert_eq!(pricing.cost(&usage), 30.5);
    }
}
