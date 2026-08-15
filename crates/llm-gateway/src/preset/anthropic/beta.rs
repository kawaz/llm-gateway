//! `anthropic-beta` ヘッダの調整。
//!
//! クライアントは自分が使う機能の beta フラグを並べて送ってくるが、
//! upstream によっては受け付けないものが混ざり、その場合リクエスト全体が
//! 400 で落ちる。落とすのは受け付けないものだけにする — 受け付ける分まで
//! 削ると、その機能が黙って効かなくなる。
//!
//! 2026-07-27 に Claude Code 2.1.220 が送る 8 個を Bedrock で 1 つずつ試した:
//!
//! | フラグ | Bedrock |
//! |---|---|
//! | `interleaved-thinking-2025-05-14` | 200 |
//! | `thinking-token-count-2026-05-13` | 200 |
//! | `context-management-2025-06-27` | 200 |
//! | `claude-code-20250219` | 200 |
//! | `oauth-2025-04-20` | 400 |
//! | `prompt-caching-scope-2026-01-05` | 400 |
//! | `advisor-tool-2026-03-01` | 400 |
//! | `extended-cache-ttl-2025-04-11` | 400 |
//!
//! この顔ぶれはクライアントの更新で変わる。同じ日に cpa のログへ残っていた
//! 束とも中身が違っていた (`fast-mode` / `redact-thinking` /
//! `token-efficient-tools` が居り、`thinking-token-count` / `advisor-tool` /
//! `extended-cache-ttl` は居なかった)。だから下の一覧は出発点にすぎず、
//! 未知のフラグが増えても止まらない仕組みが要る。

use std::collections::BTreeSet;

use crate::egress::Headers;

pub const HEADER: &str = "anthropic-beta";

/// Bedrock が受け付けないと分かっているフラグ。
///
/// 網羅ではない。ここに無いフラグで 400 になることはありうるので、
/// 呼び出し側は 400 応答からの回復手段を併せ持つこと。
pub const BEDROCK_REJECTED: &[&str] = &[
    "oauth-2025-04-20",
    "prompt-caching-scope-2026-01-05",
    "advisor-tool-2026-03-01",
    "extended-cache-ttl-2025-04-11",
];

/// beta フラグをどう扱うか。
#[derive(Debug, Clone, Default)]
pub enum Policy {
    /// クライアントが送ったまま渡す。公式 Anthropic はこれで通る。
    #[default]
    Passthrough,
    /// 挙げたフラグを落として残りを渡す。
    Deny(BTreeSet<String>),
}

impl Policy {
    /// Bedrock 向けの既定。
    pub fn bedrock() -> Self {
        Self::Deny(BEDROCK_REJECTED.iter().map(|s| (*s).to_owned()).collect())
    }

    /// 残すフラグ。
    ///
    /// 順序と表記はクライアントが送ったまま保つ。upstream が順序を見ている
    /// 可能性は低いが、変える理由も無い。
    pub fn keep(&self, value: &str) -> Vec<String> {
        let denied = match self {
            Self::Deny(denied) => Some(denied),
            Self::Passthrough => None,
        };
        value
            .split(',')
            .map(str::trim)
            .filter(|f| !f.is_empty() && !denied.is_some_and(|d| d.contains(*f)))
            .map(str::to_owned)
            .collect()
    }

    /// ヘッダ値を書き換える。全て落ちたら `None` (= ヘッダごと消す)。
    pub fn apply(&self, value: &str) -> Option<String> {
        let kept = self.keep(value);
        (!kept.is_empty()).then(|| kept.join(","))
    }

    /// ヘッダに反映し、実際に載せたフラグを返す。
    ///
    /// 返り値は 400 を受けたときの手掛かりになる。何を送ったか分からないと、
    /// どれを覚えればよいかも決められない。
    pub fn apply_to(&self, headers: &mut Headers) -> Vec<String> {
        let Some(value) = headers.get(HEADER) else {
            return Vec::new();
        };
        let kept = self.keep(value);
        match kept.is_empty() {
            true => headers.remove(HEADER),
            false => headers.set(HEADER, kept.join(",")),
        }
        kept
    }

    /// 400 応答をきっかけに、追加で落とすフラグを覚える。
    pub fn deny(&mut self, flag: &str) {
        self.deny_all([flag.to_owned()]);
    }

    /// まとめて落とす。
    pub fn deny_all(&mut self, flags: impl IntoIterator<Item = String>) {
        match self {
            Self::Deny(set) => set.extend(flags),
            Self::Passthrough => *self = Self::Deny(flags.into_iter().collect()),
        }
    }
}

/// 応答本文が「beta フラグが原因の拒否」を示しているか。
///
/// これが真なら、フラグを落とした再試行に意味がある。
pub fn is_invalid_beta_error(body: &str) -> bool {
    body.contains("invalid beta flag") || body.contains("unsupported beta")
}

/// 400 応答から、拒否されたフラグを割り出す。
///
/// 名前が書かれていれば、それだけを返す。書かれていなければ **送った全部**を
/// 返す。upstream は弾いたフラグ名を教えてくれないことがあり (実際 Bedrock の
/// 応答は `invalid beta flag` だけだった)、そのときは犯人を絞れない。
///
/// Design rationale: 絞れないときに巻き添えを避ける手は「1 つずつ送って
/// 二分探索する」しかなく、リクエスト 1 本のために何往復もすることになる。
/// 全部覚えるほうは通るフラグまで一時的に落とすが、記録には確認時刻が付いて
/// いて期限が来れば試し直す (DR-0003) ので、取りこぼしは持続しない。
pub fn blamed_flags(body: &str, sent: &[String]) -> Vec<String> {
    let named: Vec<String> = sent
        .iter()
        .filter(|flag| body.contains(flag.as_str()))
        .cloned()
        .collect();
    match named.is_empty() {
        true => sent.to_vec(),
        false => named,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code 2.1.220 が実際に送る束 (2026-07-27 に実機で観測)。
    const OBSERVED: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
thinking-token-count-2026-05-13,context-management-2025-06-27,\
prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
extended-cache-ttl-2025-04-11";

    #[test]
    fn passthrough_keeps_value_as_is() {
        assert_eq!(
            Policy::Passthrough.apply(OBSERVED).as_deref(),
            Some(OBSERVED)
        );
    }

    /// Bedrock が受け付ける 4 つだけが残る。
    #[test]
    fn bedrock_keeps_only_accepted_flags() {
        let got = Policy::bedrock().apply(OBSERVED).expect("4 should remain");
        assert_eq!(
            got,
            "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
context-management-2025-06-27,claude-code-20250219"
        );
    }

    /// 落とすと機能が消えるフラグを巻き添えにしていないか。
    /// cpa は束ごと置き換えて `context-management` を落とし、
    /// クライアントを 400 にした前科がある。
    #[test]
    fn bedrock_keeps_context_management() {
        let got = Policy::bedrock().apply(OBSERVED).unwrap();
        assert!(got.contains("context-management-2025-06-27"));
    }

    #[test]
    fn all_denied_removes_the_header() {
        let policy = Policy::bedrock();
        assert_eq!(
            policy.apply("oauth-2025-04-20,advisor-tool-2026-03-01"),
            None
        );
    }

    #[test]
    fn empty_value_yields_none() {
        assert_eq!(Policy::bedrock().apply(""), None);
        assert_eq!(Policy::bedrock().apply(" , "), None);
    }

    /// 空白入りでも拾えるが、出力は詰めた形にする。
    #[test]
    fn surrounding_spaces_are_trimmed() {
        let got = Policy::bedrock().apply(" claude-code-20250219 , oauth-2025-04-20 ");
        assert_eq!(got.as_deref(), Some("claude-code-20250219"));
    }

    /// 知らないフラグは残す (勝手に落とすと機能が消える)。
    #[test]
    fn unknown_flags_survive() {
        let got = Policy::bedrock()
            .apply("some-future-flag-2027-01-01")
            .unwrap();
        assert_eq!(got, "some-future-flag-2027-01-01");
    }

    /// 400 を受けて覚えたフラグは次から落ちる。
    #[test]
    fn learned_flag_is_dropped_next_time() {
        let mut policy = Policy::bedrock();
        let input = "some-future-flag-2027-01-01,claude-code-20250219";
        assert!(policy.apply(input).unwrap().contains("some-future-flag"));

        policy.deny("some-future-flag-2027-01-01");
        assert_eq!(policy.apply(input).as_deref(), Some("claude-code-20250219"));
    }

    /// 素通し設定でも、覚えた分だけは落とせるようになる。
    #[test]
    fn passthrough_can_learn() {
        let mut policy = Policy::Passthrough;
        policy.deny("bad-flag");
        assert_eq!(
            policy.apply("bad-flag,good-flag").as_deref(),
            Some("good-flag")
        );
    }

    /// credential が学習した分を足して使える (DR-0003)。
    #[test]
    fn learned_denials_add_to_the_upstream_default() {
        let mut policy = Policy::bedrock();
        policy.deny_all(["some-future-flag-2027-01-01".to_owned()]);

        let kept = policy.apply(OBSERVED).unwrap();
        assert!(
            !kept.contains("oauth-2025-04-20"),
            "the default entry stays dropped"
        );
        assert!(kept.contains("context-management-2025-06-27"));

        let mut passthrough = Policy::Passthrough;
        passthrough.deny_all(["a".to_owned(), "b".to_owned()]);
        assert_eq!(passthrough.apply("a,b,c").as_deref(), Some("c"));
    }

    /// ヘッダに反映すると、実際に載せたフラグが返る。
    #[test]
    fn apply_to_headers_reports_what_was_sent() {
        let mut headers = Headers::new(vec![(HEADER.to_owned(), OBSERVED.to_owned())]);
        let sent = Policy::bedrock().apply_to(&mut headers);

        assert_eq!(
            sent,
            vec![
                "interleaved-thinking-2025-05-14",
                "thinking-token-count-2026-05-13",
                "context-management-2025-06-27",
                "claude-code-20250219",
            ]
        );
        assert_eq!(headers.get(HEADER), Some(sent.join(",").as_str()));
    }

    /// 全部落ちたらヘッダごと消し、何も送っていないと報告する。
    #[test]
    fn apply_to_headers_removes_an_empty_header() {
        let mut headers = Headers::new(vec![(
            HEADER.to_owned(),
            "oauth-2025-04-20,advisor-tool-2026-03-01".to_owned(),
        )]);
        assert!(Policy::bedrock().apply_to(&mut headers).is_empty());
        assert_eq!(headers.get(HEADER), None);
    }

    /// beta を送っていないクライアントには何もしない。
    #[test]
    fn apply_to_headers_without_the_header() {
        let mut headers = Headers::default();
        assert!(Policy::bedrock().apply_to(&mut headers).is_empty());
        assert_eq!(headers.get(HEADER), None);
    }

    /// 名指しされていれば、そのフラグだけを覚える。
    #[test]
    fn blames_the_named_flag() {
        let sent = vec![
            "claude-code-20250219".to_owned(),
            "advisor-tool-2026-03-01".to_owned(),
        ];
        let body = r#"{"error":{"message":"unsupported beta: advisor-tool-2026-03-01"}}"#;
        assert_eq!(blamed_flags(body, &sent), vec!["advisor-tool-2026-03-01"]);
    }

    /// 名前が無ければ送った全部を覚える (犯人を絞れない)。
    ///
    /// 通るフラグまで一時的に落とすが、期限が来れば試し直す。
    #[test]
    fn blames_everything_when_the_body_names_nothing() {
        let sent = vec!["a-flag".to_owned(), "b-flag".to_owned()];
        let body = r#"{"error":{"message":"invalid beta flag"}}"#;
        assert_eq!(blamed_flags(body, &sent), sent);
    }

    #[test]
    fn detects_beta_rejection() {
        let bedrock_400 = r#"{"type":"error","error":{"type":"invalid_request_error","message":"invalid beta flag"}}"#;
        assert!(is_invalid_beta_error(bedrock_400));

        let other_400 = r#"{"error":{"message":"max_tokens is required"}}"#;
        assert!(!is_invalid_beta_error(other_400));
    }
}
