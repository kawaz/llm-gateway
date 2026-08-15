//! upstream からモデル一覧を取る。
//!
//! 手で書いた一覧は、新しいモデルが出るたびに追記が要る。upstream に聞けば
//! その手間が消える。取れなかったときは前回の結果を使い続ける — 一時的に
//! 繋がらないだけで、公開しているモデルが消えるのは困る。
//!
//! upstream ごとに応答の形が違うので、ここで吸収する:
//! - Anthropic 公式 `/v1/models`: `{id, created_at: "2026-07-24T00:00:00Z"}`
//! - Bedrock `/v1/models` (OpenAI 互換側): `{id: "anthropic.…", created: 1782950400}`
//!   Anthropic 互換側の `/anthropic/v1/models` は 404 なので、こちらを使う。

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::credential::Credential;
use crate::{Error, Result};

/// upstream が公開している 1 モデル。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// クライアントに見せる名前。
    pub id: String,
    /// upstream に送る名前。Bedrock は名前空間が付く。
    pub upstream_id: String,
    /// 公開日。新旧の判定に使う。取れなければ 0。
    pub created: i64,
}

/// 一覧の取り方。upstream ごとに違う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Anthropic 公式。`Authorization: Bearer` で `/v1/models`。
    Anthropic,
    /// Bedrock。`Authorization: Bearer` で OpenAI 互換側の `/v1/models`。
    /// 返る id は `anthropic.claude-opus-5` の形なので接頭辞を剥がす。
    Bedrock,
    /// ChatGPT Codex backend。account ID と client version を添えて聞く。
    OpenAiCodex,
}

pub async fn fetch(
    http: &reqwest::Client,
    flavor: Flavor,
    base_url: &str,
    credential: &Credential,
) -> Result<Vec<Model>> {
    let root = base_url.trim_end_matches('/');
    let request = match flavor {
        Flavor::Anthropic => {
            let url = format!("{root}/v1/models");
            http.get(&url)
                .header("authorization", credential.bearer())
                .header("anthropic-version", "2023-06-01")
        }
        Flavor::Bedrock => {
            // base_url は `…/anthropic` を指しているが、モデル一覧は
            // その隣の `/v1/models` にしかない。
            let url = format!("{}/v1/models", root.trim_end_matches("/anthropic"));
            http.get(&url)
                .header("authorization", format!("Bearer {}", credential.api_key()))
                .header("anthropic-version", "2023-06-01")
        }
        Flavor::OpenAiCodex => {
            let url = format!("{root}/models?client_version={}", env!("CARGO_PKG_VERSION"));
            let account_id = credential
                .account_id
                .as_deref()
                .ok_or_else(|| Error::Credential {
                    id: credential.id.to_string(),
                    reason: "no ChatGPT account identifier".to_owned(),
                })?;
            http.get(&url)
                .header("authorization", credential.bearer())
                .header("chatgpt-account-id", account_id)
        }
    };

    let resp = request
        .send()
        .await
        .map_err(|e| Error::UpstreamUnreachable {
            provider: credential.id.to_string(),
            source: e,
        })?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| Error::Config(format!("could not read the model list: {e}")))?;

    if !status.is_success() {
        return Err(Error::UpstreamStatus {
            provider: credential.id.to_string(),
            status: status.as_u16(),
            body: text.chars().take(200).collect(),
        });
    }

    parse(flavor, &text)
}

pub fn parse(flavor: Flavor, body: &str) -> Result<Vec<Model>> {
    if flavor == Flavor::OpenAiCodex {
        return parse_codex(body);
    }

    #[derive(Deserialize)]
    struct List {
        data: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        id: String,
        /// Anthropic は RFC 3339 の文字列。
        #[serde(default)]
        created_at: Option<String>,
        /// Bedrock は unix 秒。
        #[serde(default)]
        created: Option<i64>,
        /// Bedrock だけが持つ。`available` 以外は使えない。
        #[serde(default)]
        status: Option<String>,
    }

    let list: List = serde_json::from_str(body)?;
    let mut models = Vec::new();

    for e in list.data {
        if e.status.as_deref().is_some_and(|s| s != "available") {
            continue;
        }
        let (id, upstream_id) = match flavor {
            Flavor::Anthropic => (e.id.clone(), e.id),
            Flavor::Bedrock => match e.id.strip_prefix("anthropic.") {
                // Bedrock には OpenAI 系も並ぶ。Anthropic 互換の口に
                // 投げられるのは anthropic.* だけなので、他は拾わない。
                Some(short) => (short.to_owned(), e.id.clone()),
                None => continue,
            },
            Flavor::OpenAiCodex => unreachable!("handled before the shared parser"),
        };
        models.push(Model {
            id,
            upstream_id,
            created: e
                .created
                .or_else(|| e.created_at.as_deref().and_then(parse_date))
                .unwrap_or(0),
        });
    }

    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

fn parse_codex(body: &str) -> Result<Vec<Model>> {
    #[derive(Deserialize)]
    struct List {
        models: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        model: Option<String>,
    }

    let list: List = serde_json::from_str(body)?;
    let mut models = Vec::new();
    for entry in list.models {
        let Some(id) = entry.id.or(entry.slug).or(entry.model) else {
            continue;
        };
        models.push(Model {
            upstream_id: id.clone(),
            id,
            created: 0,
        });
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

/// `2026-07-24T00:00:00Z` を unix 秒にする。日付部分だけで足りる。
fn parse_date(s: &str) -> Option<i64> {
    let (y, rest) = s.split_once('-')?;
    let (m, rest) = rest.split_once('-')?;
    let d = rest.get(..2)?;
    let (y, m, d) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    Some(days_from_civil(y, m, d) * 86_400)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// エイリアスを実際のモデル名に解決する。
///
/// `opus` のような短い名前を、`claude-opus-*` に当たるもののうち
/// **一番新しいもの**へ向ける。新しいモデルが出たら自動で乗り換わる。
pub fn resolve_alias(pattern: &str, available: &[Model]) -> Option<String> {
    available
        .iter()
        .filter(|m| crate::pattern::matches(pattern, &m.id))
        // 日付が同じなら名前の大きい方 (新しい版が後ろに来る想定)。
        .max_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)))
        .map(|m| m.id.clone())
}

/// 短い名前の既定。
///
/// **空**。エイリアスは設定に書く。コードに既定を置くと、設定を読んだだけでは
/// 何が公開されるか分からなくなり、消したいときにコードを触ることになる。
pub fn default_aliases() -> BTreeMap<String, String> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-27 に Anthropic 公式が実際に返した応答。
    const ANTHROPIC_BODY: &str = r#"{"data":[
        {"id":"claude-opus-4-1-20250805","created_at":"2025-08-05T00:00:00Z"},
        {"id":"claude-sonnet-4-5-20250929","created_at":"2025-09-29T00:00:00Z"},
        {"id":"claude-haiku-4-5-20251001","created_at":"2025-10-15T00:00:00Z"},
        {"id":"claude-opus-4-5-20251101","created_at":"2025-11-24T00:00:00Z"},
        {"id":"claude-opus-4-6","created_at":"2026-02-04T00:00:00Z"},
        {"id":"claude-sonnet-4-6","created_at":"2026-02-17T00:00:00Z"},
        {"id":"claude-opus-4-7","created_at":"2026-04-14T00:00:00Z"},
        {"id":"claude-opus-4-8","created_at":"2026-05-28T00:00:00Z"},
        {"id":"claude-fable-5","created_at":"2026-06-07T00:00:00Z"},
        {"id":"claude-sonnet-5","created_at":"2026-06-29T00:00:00Z"},
        {"id":"claude-opus-5","created_at":"2026-07-24T00:00:00Z"}
    ]}"#;

    /// 同日に Bedrock が返した応答 (OpenAI 系も混ざる)。
    const BEDROCK_BODY: &str = r#"{"data":[
        {"id":"anthropic.claude-fable-5","created":1780272000,"status":"available"},
        {"id":"anthropic.claude-opus-5","created":1782950400,"status":"available"},
        {"id":"anthropic.claude-haiku-4-5","created":1759276800,"status":"available"},
        {"id":"openai.gpt-oss-120b","created":1750000000,"status":"available"},
        {"id":"anthropic.claude-opus-4-8","created":1770000000,"status":"available"}
    ]}"#;

    fn anthropic() -> Vec<Model> {
        parse(Flavor::Anthropic, ANTHROPIC_BODY).unwrap()
    }

    #[test]
    fn parses_anthropic_list() {
        let models = anthropic();
        assert_eq!(models.len(), 11);
        let opus5 = models.iter().find(|m| m.id == "claude-opus-5").unwrap();
        assert_eq!(
            opus5.upstream_id, "claude-opus-5",
            "official entries use the name as-is"
        );
        assert!(opus5.created > 0, "created_at yields a date");
    }

    /// Codex catalog は `models` 配列の slug / id を client と upstream の名前に使う。
    #[test]
    fn parses_codex_catalog() {
        let models = parse(
            Flavor::OpenAiCodex,
            r#"{"models":[{"slug":"gpt-5.3-codex"},{"id":"gpt-5.2-codex"},{"model":"codex-mini-latest"}]}"#,
        )
        .unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["codex-mini-latest", "gpt-5.2-codex", "gpt-5.3-codex"]
        );
        assert!(models.iter().all(|model| model.id == model.upstream_id));
    }

    /// Bedrock は名前空間が付く。クライアントには剥がした名前を見せ、
    /// upstream には元の名前で送る。
    #[test]
    fn strips_bedrock_namespace() {
        let models = parse(Flavor::Bedrock, BEDROCK_BODY).unwrap();
        let fable = models.iter().find(|m| m.id == "claude-fable-5").unwrap();
        assert_eq!(fable.upstream_id, "anthropic.claude-fable-5");
    }

    /// Anthropic 互換の口に投げられないものは拾わない。
    #[test]
    fn skips_non_anthropic_models_on_bedrock() {
        let models = parse(Flavor::Bedrock, BEDROCK_BODY).unwrap();
        assert!(
            !models.iter().any(|m| m.id.contains("gpt")),
            "OpenAI-family models are unusable through the Anthropic-compatible endpoint"
        );
        assert_eq!(models.len(), 4);
    }

    /// 使えない状態のものは出さない。
    #[test]
    fn skips_unavailable_models() {
        let body = r#"{"data":[
            {"id":"anthropic.claude-opus-5","created":1,"status":"available"},
            {"id":"anthropic.claude-opus-9","created":2,"status":"coming_soon"}
        ]}"#;
        let models = parse(Flavor::Bedrock, body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-opus-5");
    }

    /// エイリアスは一番新しいものに向く。
    #[test]
    fn alias_picks_the_newest() {
        let models = anthropic();
        assert_eq!(
            resolve_alias("claude-opus-*", &models).as_deref(),
            Some("claude-opus-5"),
            "5, not 4-8 or 4-1"
        );
        assert_eq!(
            resolve_alias("claude-sonnet-*", &models).as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            resolve_alias("claude-haiku-*", &models).as_deref(),
            Some("claude-haiku-4-5-20251001")
        );
    }

    /// 新しいモデルが出たら、設定を触らずに乗り換わる。
    #[test]
    fn alias_follows_new_releases() {
        let mut models = anthropic();
        assert_eq!(
            resolve_alias("claude-opus-*", &models).as_deref(),
            Some("claude-opus-5")
        );

        models.push(Model {
            id: "claude-opus-6".into(),
            upstream_id: "claude-opus-6".into(),
            created: 1_800_000_000,
        });
        assert_eq!(
            resolve_alias("claude-opus-*", &models).as_deref(),
            Some("claude-opus-6"),
            "follows along without touching the config"
        );
    }

    /// 日付が無ければ名前の大きい方を選ぶ (日付が取れない upstream 向け)。
    #[test]
    fn alias_falls_back_to_name_order() {
        let models = vec![
            Model {
                id: "claude-opus-4".into(),
                upstream_id: "claude-opus-4".into(),
                created: 0,
            },
            Model {
                id: "claude-opus-5".into(),
                upstream_id: "claude-opus-5".into(),
                created: 0,
            },
        ];
        assert_eq!(
            resolve_alias("claude-opus-*", &models).as_deref(),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn alias_without_match_resolves_to_nothing() {
        assert_eq!(resolve_alias("claude-nonexistent-*", &anthropic()), None);
    }

    /// エイリアスはコードに持たない。設定に書く。
    #[test]
    fn no_aliases_are_built_in() {
        assert!(
            default_aliases().is_empty(),
            "with a code default present, reading the config alone doesn't reveal what's exposed"
        );
    }

    /// 実運用で使うパターンが、実際の一覧で意図どおり解決する。
    #[test]
    fn typical_patterns_resolve_to_newest() {
        let models = anthropic();
        let cases = [
            ("claude-fable-*", "claude-fable-5"),
            ("claude-opus-*", "claude-opus-5"),
            ("claude-sonnet-*", "claude-sonnet-5"),
            ("claude-haiku-*", "claude-haiku-4-5-20251001"),
        ];
        for (pattern, want) in cases {
            assert_eq!(
                resolve_alias(pattern, &models).as_deref(),
                Some(want),
                "{pattern} → {want}"
            );
        }
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_date("2026-07-24T00:00:00Z"), Some(1_784_851_200));
        assert_eq!(parse_date("broken"), None);
    }

    /// 形が違っても panic しない。
    #[test]
    fn rejects_malformed_body() {
        assert!(parse(Flavor::Anthropic, "not json").is_err());
        assert!(parse(Flavor::Anthropic, r#"{"data":"wrong"}"#).is_err());
        assert!(
            parse(Flavor::Anthropic, r#"{"data":[]}"#)
                .unwrap()
                .is_empty()
        );
    }

    /// 日付が無い応答でも読める。
    #[test]
    fn missing_dates_are_tolerated() {
        let models = parse(Flavor::Anthropic, r#"{"data":[{"id":"claude-opus-5"}]}"#).unwrap();
        assert_eq!(models[0].created, 0);
    }
}
