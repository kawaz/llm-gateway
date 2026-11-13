//! Anthropic Messages API を話す upstream の方言一式。
//!
//! この方言を話すのは公式だけではない。Bedrock ([`crate::preset::bedrock`]) は
//! 認証だけ差し替えて同じ [`AnthropicWire`] を使い、別 gateway への素通し
//! ([`crate::preset::relay`]) は認証を載せずに同じ Wire を使う。方言を持つ側が
//! 1 つなので、公式に効く直しはそのまま両方へ届く。
//!
//! 内部正規形が Anthropic Messages 形式 (DR-0014 §5) なので、この Wire の
//! 変換は「接続先と付随ヘッダを合わせる」だけで済む。

mod account_api;
mod auth;
pub mod beta;
mod metering;
mod negotiation;
mod quota_api;
mod wire;

use std::sync::Arc;

pub use account_api::OauthProfile;
pub use auth::OauthBearer;
pub use metering::AnthropicMetering;
pub use negotiation::BetaFlags;
pub use quota_api::OauthUsage;
pub use wire::AnthropicWire;

use crate::credential::Credential;
use crate::provider::Preset;
use crate::quota::Support;

/// OAuth の照会口 (`/api/oauth/*`) に聞くのを諦めるまで。
///
/// 枠照会は人が見て待つ利用状況の一覧から呼ばれるので、上限を短く切る。
/// 相手が黙っていても他の credential の分まで返らなくなることはない。
const OAUTH_GET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// OAuth の照会口に GET で聞き、成功した応答の本文を返す。
///
/// 照会口はどれも公開ドキュメントに無いので、届かない・失敗した・本文が
/// 読めないものは**情報なし** (`None`) に落とし、gateway の他の動きを
/// 巻き込まない。何が起きたかはログに残す。
async fn oauth_get(
    http: &reqwest::Client,
    base_url: &str,
    path: &str,
    credential: &Credential,
) -> Option<String> {
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .header("authorization", credential.bearer())
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .timeout(OAUTH_GET_TIMEOUT)
        .send()
        .await;

    let resp = match resp {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(credential = %credential.id, path, %e, "cannot reach the OAuth endpoint");
            return None;
        }
    };
    let status = resp.status();
    let body = match resp.text().await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(credential = %credential.id, path, %e, "cannot read the OAuth endpoint response");
            return None;
        }
    };
    if !status.is_success() {
        tracing::warn!(
            credential = %credential.id,
            path,
            status = status.as_u16(),
            "the OAuth endpoint refused the query"
        );
        return None;
    }
    Some(body)
}

/// 枠の呼び名から、その枠が回る周期 (秒) を起こす (DR-0018 §6)。
///
/// Anthropic は窓の長さを数値では返さない。返るのは呼び名だけで、応答ヘッダ
/// なら欄名 (`...-5h-...` / `...-7d-...`)、枠照会 API なら `kind`
/// (`session` / `weekly_all` / `weekly_scoped`) がそれにあたる。周期が
/// 呼び名に埋まっているのはこの provider の事情なので、対応表はここに閉じる。
///
/// 知らない呼び名は `None`。推測で周期を埋めると、使い切りの判定 (DR-0018)
/// が誤った窓長を基準にしてしまう。
fn window_seconds(kind: &str) -> Option<u64> {
    const HOUR: u64 = 60 * 60;
    match kind {
        "5h" | "session" => Some(5 * HOUR),
        "7d" => Some(7 * 24 * HOUR),
        // 週次の枠は掛かる範囲 (全体 / モデル別) が違うだけで、周期は同じ。
        kind if kind.starts_with("weekly") => Some(7 * 24 * HOUR),
        _ => None,
    }
}

/// Anthropic 公式の preset。
///
/// サブスクの OAuth token をそのまま載せ、トークンを消費しない枠照会 API を
/// 持つ (DR-0007)。組織ごと断られたときに理由を聞く口 (profile) も持つ。beta フラグは公式なら全部通るので、既定は素通し。
///
/// 枠は応答ヘッダに載るので、まだ観測が無い状態は「取れるがまだ」と言える。
pub fn official(name: &str, wire: Arc<AnthropicWire>) -> Preset {
    let quota_api = OauthUsage::new(wire.base_url());
    let account_api = OauthProfile::new(wire.base_url());
    Preset::new(
        name,
        Arc::new(OauthBearer::new(name)),
        wire,
        Arc::new(AnthropicMetering),
    )
    .with_quota_api(Arc::new(quota_api))
    .with_account_api(Arc::new(account_api))
    .with_negotiation(Arc::new(BetaFlags::new(beta::Policy::Passthrough)))
    .with_quota_support(Support::Unobserved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn wire() -> Arc<AnthropicWire> {
        Arc::new(AnthropicWire::new(
            "claude-personal",
            "https://api.anthropic.com",
            BTreeMap::new(),
        ))
    }

    /// 公式は枠照会 API を持つ。持たない preset との差は型に出る (DR-0014 §2)。
    #[test]
    fn official_has_a_quota_api() {
        let preset = official("claude-personal", wire());

        assert_eq!(preset.name(), "claude-personal");
        assert!(preset.quota_api().is_some());
        assert!(
            preset.negotiation().is_some(),
            "beta negotiation is required on every route"
        );
        assert_eq!(
            preset.quota_support(),
            Support::Unobserved,
            "quota rides on the response headers, so using it makes it visible"
        );
    }

    /// 公式は、組織ごと断られた理由を聞ける口も持つ。
    #[test]
    fn official_can_ask_why_the_org_is_refused() {
        assert!(official("claude-personal", wire()).account_api().is_some());
    }

    /// モデル別の週の枠を使い切った 429 は、応答の窓が `allowed` のままで
    /// `retry-after` だけが週のリセットまでの秒数を指す (実測 2026-11-12、
    /// fable-5-2 で 208521 秒)。こうして付いたモデル単位の印も、枠照会 API が
    /// そのモデル群の枠は使い切っていないと答えたら外れる。外れないと、
    /// 利用者が枠を外でリセットしても、週のリセットまで締め出したままになる。
    #[test]
    fn an_open_scoped_limit_lifts_the_mark_of_a_model_it_covers() {
        use crate::egress::Headers;
        use crate::quota::QuotaLimit;

        const NOW: i64 = 1_800_000_000;
        const FABLE: &str = "claude-fable-5-2";
        const WEEK_RESET: i64 = NOW + 208_521;

        let preset = official("claude-personal", wire());
        let refused = Headers::new(
            [
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("retry-after", "208521"),
            ]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        );
        let denial = preset
            .reject(429, &refused, None, FABLE, NOW)
            .expect("a 429 holds the model back");
        assert_eq!(denial.until, WEEK_RESET, "precondition: held for days");
        assert_ne!(
            preset.availability(FABLE, NOW),
            crate::denial::Availability::Ready
        );

        let fable = |percent: f64| QuotaLimit {
            kind: "weekly_scoped".to_owned(),
            percent,
            severity: None,
            resets_at: Some(crate::credential::time::to_unix_ms(WEEK_RESET)),
            model: Some("Fable".to_owned()),
            model_id: None,
            window_seconds: window_seconds("weekly_scoped"),
            is_active: true,
        };

        let later = NOW + 60 * 60;
        preset.apply_quota(&[fable(100.0)], later);
        assert_ne!(
            preset.availability(FABLE, later),
            crate::denial::Availability::Ready,
            "still exhausted: the route stays closed"
        );

        let after_reset = later + 60 * 60;
        preset.apply_quota(&[fable(0.0)], after_reset);
        assert_eq!(
            preset.availability(FABLE, after_reset),
            crate::denial::Availability::Ready,
            "the scoped limit was reset elsewhere, so the model mark must go too"
        );
    }
}
