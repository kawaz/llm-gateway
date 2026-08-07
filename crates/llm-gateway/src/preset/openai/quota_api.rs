//! ChatGPT サブスク枠を token 消費なしで聞く口。

use serde::Deserialize;

use crate::credential::Credential;
use crate::credential::time::format_rfc3339;
use crate::denial::{Denial, RESET_SLACK, Reason, Scope};
use crate::egress::BoxFuture;
use crate::provider::{ProbeRequest, QuotaApi};
use crate::quota::QuotaLimit;
use crate::{Error, Result};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct WhamUsage {
    base_url: String,
}

impl WhamUsage {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    fn usage_url(&self) -> String {
        let root = self
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/codex");
        format!("{root}/wham/usage")
    }
}

impl QuotaApi for WhamUsage {
    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<Vec<QuotaLimit>>> {
        Box::pin(async move {
            let account_id = credential
                .account_id
                .as_deref()
                .ok_or_else(|| Error::Credential {
                    id: credential.id.to_string(),
                    reason: "ChatGPT のアカウント識別子がありません".to_owned(),
                })?;
            let response = http
                .get(self.usage_url())
                .header("authorization", credential.bearer())
                .header("chatgpt-account-id", account_id)
                .timeout(TIMEOUT)
                .send()
                .await
                .map_err(|source| Error::UpstreamUnreachable {
                    provider: credential.id.to_string(),
                    source,
                })?;
            let status = response.status();
            let body = response.text().await.map_err(|error| {
                Error::Config(format!("ChatGPT quota の応答を読めません: {error}"))
            })?;
            if !status.is_success() {
                return Err(Error::UpstreamStatus {
                    provider: credential.id.to_string(),
                    status: status.as_u16(),
                    body: String::new(),
                });
            }
            parse(&body)
                .ok_or_else(|| Error::Config("ChatGPT quota の応答を解釈できません".to_owned()))
        })
    }

    fn denials(&self, limits: &[QuotaLimit], _now: i64) -> Vec<(Scope, Option<Denial>)> {
        limits
            .iter()
            .filter_map(|limit| {
                let scope = Scope::Everything;
                if limit.percent < 100.0 {
                    return Some((scope, None));
                }
                let reset = limit
                    .resets_at
                    .as_deref()
                    .and_then(crate::credential::time::parse_rfc3339)?;
                Some((
                    scope.clone(),
                    Some(Denial {
                        until: reset + RESET_SLACK,
                        reason: Reason::Limited,
                        scope,
                    }),
                ))
            })
            .collect()
    }

    /// quota endpoint 自体が token を消費せずに答えるため、推論 probe は要らない。
    fn probe_request(&self) -> Option<ProbeRequest> {
        None
    }
}

fn parse(body: &str) -> Option<Vec<QuotaLimit>> {
    #[derive(Deserialize)]
    struct Response {
        rate_limit: RateLimit,
    }
    #[derive(Deserialize)]
    struct RateLimit {
        #[serde(default)]
        primary_window: Option<Window>,
        #[serde(default)]
        secondary_window: Option<Window>,
    }
    #[derive(Deserialize)]
    struct Window {
        used_percent: f64,
        #[serde(default)]
        reset_at: Option<i64>,
    }

    let parsed: Response = serde_json::from_str(body).ok()?;
    let mut limits = Vec::new();
    for (kind, window) in [
        ("primary", parsed.rate_limit.primary_window),
        ("secondary", parsed.rate_limit.secondary_window),
    ] {
        let Some(window) = window else {
            continue;
        };
        limits.push(QuotaLimit {
            kind: kind.to_owned(),
            percent: window.used_percent,
            severity: (window.used_percent >= 100.0).then(|| "critical".to_owned()),
            resets_at: window.reset_at.map(format_rfc3339),
            model: None,
            model_id: None,
            is_active: true,
        });
    }
    Some(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// primary / secondary window を upstream の区分名のまま保持する。
    #[test]
    fn parses_both_subscription_windows() {
        let limits = parse(
            r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_at":1800001000},"secondary_window":{"used_percent":100,"limit_window_seconds":604800,"reset_at":1800005000}}}"#,
        )
        .unwrap();
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0].kind, "primary");
        assert_eq!(limits[0].percent, 25.0);
        assert_eq!(limits[1].severity.as_deref(), Some("critical"));
        assert!(limits[1].resets_at.is_some());
    }

    /// 専用 quota endpoint があるので、枠確認のための推論 request は作らない。
    #[test]
    fn does_not_offer_a_consuming_probe() {
        assert!(
            WhamUsage::new("https://chatgpt.com/backend-api/codex")
                .probe_request()
                .is_none()
        );
    }
}
