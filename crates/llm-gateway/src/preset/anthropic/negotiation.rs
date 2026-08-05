//! `anthropic-beta` の交渉を core の任意 capability として差し出す (DR-0003)。
//!
//! upstream ごとに受け付けるフラグが違い、しかも**一覧は誰も公開していない**。
//! だから既定の拒否リストから始めて、400 を受けたら責めるべきフラグを割り出し、
//! credential へ覚えさせる。覚える先は転送側が持つので、ここが答えるのは
//! 「何を載せたか」と「何を責めるか」の 2 つだけ。

use crate::egress::Headers;
use crate::provider::Negotiation;

use super::beta;

/// beta フラグの取捨を担う。
pub struct BetaFlags {
    /// この upstream へ送るときの既定。
    policy: beta::Policy,
}

impl BetaFlags {
    pub fn new(policy: beta::Policy) -> Self {
        Self { policy }
    }
}

impl Negotiation for BetaFlags {
    /// 既定の拒否リストに、この credential で学習した分を足して落とす。
    ///
    /// 同じ upstream でも region や契約で受け付ける beta が違うので、学習結果は
    /// 認証情報ごとに持つ (DR-0003)。
    fn prepare(&self, headers: &mut Headers, learned: &[String]) -> Vec<String> {
        let mut policy = self.policy.clone();
        policy.deny_all(learned.iter().cloned());
        policy.apply_to(headers)
    }

    fn blame(&self, body: &str, sent: &[String]) -> Option<Vec<String>> {
        if sent.is_empty() || !beta::is_invalid_beta_error(body) {
            return None;
        }
        Some(beta::blamed_flags(body, sent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code 2.1.220 が実際に送る beta の束。
    const CLIENT_BETA: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
claude-code-20250219,advisor-tool-2026-03-01";

    fn headers() -> Headers {
        Headers::new(vec![("anthropic-beta".to_owned(), CLIENT_BETA.to_owned())])
    }

    /// 既定が素通しなら、クライアントの束はそのまま載る。
    #[test]
    fn passthrough_sends_what_the_client_asked_for() {
        let mut sending = headers();
        let sent = BetaFlags::new(beta::Policy::Passthrough).prepare(&mut sending, &[]);

        assert_eq!(sending.get("anthropic-beta"), Some(CLIENT_BETA));
        assert_eq!(sent.len(), 4, "載せた分を返す: {sent:?}");
    }

    /// 学習済みのフラグは、1 本目から落ちる。
    #[test]
    fn learned_flags_are_dropped_before_sending() {
        let mut sending = headers();
        let sent = BetaFlags::new(beta::Policy::Passthrough)
            .prepare(&mut sending, &["advisor-tool-2026-03-01".to_owned()]);

        let value = sending.get("anthropic-beta").expect("残るものがある");
        assert!(!value.contains("advisor-tool-2026-03-01"), "{value}");
        assert!(value.contains("oauth-2025-04-20"), "{value}");
        assert!(!sent.contains(&"advisor-tool-2026-03-01".to_owned()));
    }

    /// 既定の拒否リストと学習分は足し合わさる。
    #[test]
    fn defaults_and_learned_flags_add_up() {
        let mut sending = headers();
        BetaFlags::new(beta::Policy::bedrock())
            .prepare(&mut sending, &["claude-code-20250219".to_owned()]);

        let value = sending.get("anthropic-beta").expect("残るものがある");
        assert_eq!(
            value, "interleaved-thinking-2025-05-14",
            "既定で落ちる分と、覚えた分の両方が落ちる"
        );
    }

    /// 全部落ちたらヘッダごと消す (空の値を送らない)。
    #[test]
    fn nothing_left_removes_the_header() {
        let mut sending = Headers::new(vec![(
            "anthropic-beta".to_owned(),
            "oauth-2025-04-20,advisor-tool-2026-03-01".to_owned(),
        )]);
        let sent = BetaFlags::new(beta::Policy::bedrock()).prepare(&mut sending, &[]);

        assert_eq!(sending.get("anthropic-beta"), None);
        assert!(sent.is_empty());
    }

    /// 名指しされていれば、そのフラグだけを責める。
    #[test]
    fn a_named_flag_is_blamed_alone() {
        let sent = vec![
            "oauth-2025-04-20".to_owned(),
            "advisor-tool-2026-03-01".to_owned(),
        ];
        let blamed = BetaFlags::new(beta::Policy::Passthrough)
            .blame(
                r#"{"error":{"message":"unsupported beta: advisor-tool-2026-03-01"}}"#,
                &sent,
            )
            .expect("交渉のやり直しを求めている");

        assert_eq!(blamed, vec!["advisor-tool-2026-03-01"]);
    }

    /// 名前が書かれていなければ、送った全部を責める。
    ///
    /// 犯人を絞る手は「1 つずつ送って二分探索する」しかなく、リクエスト 1 本の
    /// ために何往復もすることになる (DR-0003)。
    #[test]
    fn an_unnamed_rejection_blames_everything_sent() {
        let sent = vec!["a".to_owned(), "b".to_owned()];
        let blamed = BetaFlags::new(beta::Policy::Passthrough)
            .blame(r#"{"error":{"message":"invalid beta flag"}}"#, &sent)
            .expect("交渉のやり直しを求めている");

        assert_eq!(blamed, sent);
    }

    /// beta と関係ない失敗、または何も載せていない失敗は交渉の話ではない。
    #[test]
    fn an_unrelated_failure_is_not_a_negotiation() {
        let negotiation = BetaFlags::new(beta::Policy::Passthrough);
        let sent = vec!["a".to_owned()];

        assert_eq!(negotiation.blame(r#"{"error":"max_tokens"}"#, &sent), None);
        assert_eq!(
            negotiation.blame(r#"{"error":"invalid beta flag"}"#, &[]),
            None,
            "載せていないものは責められない"
        );
    }
}
