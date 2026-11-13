//! 組織ごと OAuth を断られたときに、その理由を聞く口。
//!
//! 断りの応答 (403) は「この組織には OAuth の利用が許可されていない」までしか
//! 言わない。`/api/oauth/profile` は断られている間も読めて (実測 2026-11-13)、
//! サブスクリプションの状態と、支払いが滞っていれば未払いの請求を払う先を
//! 返す。
//!
//! この口は Anthropic の OAuth にだけあり、Bedrock には無い。「無い」ことは
//! 空実装ではなく [`crate::provider::Preset`] が `None` を返すことで示す。
//!
//! 公開ドキュメントにも無い。返る形が変われば読めなくなるので、読めない
//! ものは**情報なし**に落とし、gateway の他の動きを巻き込まない。応答には
//! 個人と組織を識別する欄 (メールアドレス・氏名・組織名) も載るが、使うのは
//! 理由の判断に要る欄だけで、それ以外は読まない。

use serde::Deserialize;

use crate::credential::Credential;
use crate::denial::Standing;
use crate::egress::BoxFuture;
use crate::provider::AccountApi;
use crate::{Error, Result};

/// 支払いが滞っていることを表す `subscription_status` の値 (実測 2026-11-13)。
const PAST_DUE: &str = "past_due";

/// `/api/oauth/profile` で口座の状態を聞く。
pub struct OauthProfile {
    base_url: String,
}

impl OauthProfile {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl AccountApi for OauthProfile {
    fn standing<'a>(
        &'a self,
        http: &'a reqwest::Client,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<Standing>> {
        Box::pin(async move {
            let body =
                super::oauth_get(http, &self.base_url, "/api/oauth/profile", credential).await;
            body.as_deref().and_then(parse).ok_or_else(|| {
                Error::upstream_response(
                    "Anthropic profile API",
                    "could not fetch or parse the profile response".to_owned(),
                )
            })
        })
    }
}

/// 応答を読む。知っている形でなければ `None`。
fn parse(body: &str) -> Option<Standing> {
    /// 応答のうち、こちらが使う部分だけ。
    #[derive(Deserialize)]
    struct Response {
        organization: Organization,
    }
    #[derive(Deserialize)]
    struct Organization {
        #[serde(default)]
        subscription_status: Option<String>,
        #[serde(default)]
        payment_auth_hosted_invoice_url: Option<String>,
    }

    let parsed: Response = match serde_json::from_str(body) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(%e, "the profile response shape has changed");
            return None;
        }
    };
    let organization = parsed.organization;
    let hint = hint_for(
        organization.subscription_status.as_deref(),
        organization.payment_auth_hosted_invoice_url.as_deref(),
    );
    Some(Standing {
        subscription_status: organization.subscription_status,
        hint,
    })
}

/// 状態から言える対処。言えることが無ければ `None`。
///
/// 対処を言えるのは意味を確かめた値 (`past_due`) だけ。正常なアカウントは
/// `active` を返す (実測 2026-11-13) が、組織ごと断られている間にそれが返る
/// なら原因は契約の外にあり、こちらから言える対処は無い。他の値は決め打ち
/// せず、状態をそのまま見せるに留める。
///
/// 払う先は https の URL だけを添える。上流の値をそのまま人に見せるので、
/// 開いてよいか分からないものは載せず、請求の設定を見るよう促す。
fn hint_for(subscription_status: Option<&str>, invoice_url: Option<&str>) -> Option<String> {
    if subscription_status != Some(PAST_DUE) {
        return None;
    }
    Some(
        match invoice_url.filter(|url| url.starts_with("https://")) {
            Some(url) => format!("the subscription payment is past due; pay the overdue invoice at {url}"),
            None => "the subscription payment is past due; pay the overdue invoice in the account's billing settings".to_owned(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 支払いが滞って止められているアカウントの応答 (2026-11-13 実測) の
    /// 抜粋。識別子の欄は値を差し替えてある。使わない欄も、読み飛ばせるか
    /// 確かめるために残してある。
    const PAST_DUE_SAMPLE: &str = r#"{
      "account": {"full_name": "Example User", "email": "user@example.com"},
      "organization": {
        "name": "Example Org",
        "billing_type": "stripe_subscription",
        "plan_display_name": "Ultra",
        "subscription_status": "past_due",
        "payment_auth_hosted_invoice_url": "https://claude.ai/settings/billing?action=pay_overdue_invoice"
      }
    }"#;

    const INVOICE_URL: &str = "https://claude.ai/settings/billing?action=pay_overdue_invoice";

    #[test]
    fn a_past_due_subscription_points_to_the_overdue_invoice() {
        let standing = parse(PAST_DUE_SAMPLE).expect("a known shape");
        assert_eq!(standing.subscription_status.as_deref(), Some("past_due"));
        let hint = standing.hint.expect("past_due has a known remedy");
        assert!(hint.contains("past due"), "{hint}");
        assert!(hint.contains(INVOICE_URL), "{hint}");
    }

    /// 正常なアカウントの応答 (2026-11-13 実測) の抜粋。払う先は `null`。
    const ACTIVE_SAMPLE: &str = r#"{
      "organization": {
        "organization_type": "claude_max",
        "billing_type": "stripe_subscription",
        "plan_display_name": "Ultra",
        "subscription_status": "active",
        "payment_auth_hosted_invoice_url": null
      }
    }"#;

    /// 正常な状態は値をそのまま見せ、対処は言わない (推定の hint に任せる)。
    #[test]
    fn an_active_subscription_is_shown_without_a_remedy() {
        assert_eq!(
            parse(ACTIVE_SAMPLE),
            Some(Standing {
                subscription_status: Some("active".to_owned()),
                hint: None,
            })
        );
    }

    /// 払う先が返らなくても、支払いが滞っていることは言える。
    #[test]
    fn a_past_due_subscription_without_an_invoice_url_still_says_so() {
        let standing = parse(r#"{"organization": {"subscription_status": "past_due"}}"#).unwrap();
        let hint = standing.hint.unwrap();
        assert!(hint.contains("past due"), "{hint}");
        assert!(hint.contains("billing settings"), "{hint}");
    }

    /// 開いてよいか分からない払い先は人に見せない。
    #[test]
    fn a_non_https_invoice_url_is_not_shown() {
        let standing = parse(
            r#"{"organization": {"subscription_status": "past_due",
                "payment_auth_hosted_invoice_url": "javascript:alert(1)"}}"#,
        )
        .unwrap();
        let hint = standing.hint.unwrap();
        assert!(!hint.contains("javascript"), "{hint}");
    }

    /// 意味を確かめていない値は、状態だけを見せて対処は言わない。
    #[test]
    fn an_unverified_status_is_shown_without_a_remedy() {
        let standing = parse(
            r#"{"organization": {"subscription_status": "something_new",
                "payment_auth_hosted_invoice_url": "https://example.com/pay"}}"#,
        )
        .unwrap();
        assert_eq!(
            standing.subscription_status.as_deref(),
            Some("something_new")
        );
        assert_eq!(standing.hint, None);
    }

    /// 状態の欄が無くても形としては読める。言えることが無いだけ。
    #[test]
    fn a_profile_without_a_status_says_nothing() {
        assert_eq!(parse(r#"{"organization": {}}"#), Some(Standing::default()));
    }

    /// 知らない形は情報なしに落とす。読めないものを推測で埋めない。
    #[test]
    fn an_unknown_shape_yields_nothing() {
        for body in ["", "{}", "null", r#"{"organization": "x"}"#, "<html>"] {
            assert_eq!(parse(body), None, "{body}");
        }
    }

    /// 届かなければ失敗を返す。呼び出し側はそれを「情報なし」として扱う。
    #[tokio::test]
    async fn an_unreachable_host_is_an_error() {
        // 誰も待ち受けていない先。接続が拒まれた時点で戻る。
        let api = OauthProfile::new("http://127.0.0.1:1");
        let result = api
            .standing(&reqwest::Client::new(), &Credential::for_test("tok"))
            .await;

        assert!(result.is_err());
    }
}
