//! 枠をトークンを消費せずに聞く口 (DR-0007)。
//!
//! 転送の応答ヘッダから読める枠は 5 時間 / 7 日の 2 つだけで、**モデル別の枠は
//! 載らない**。`/api/oauth/usage` は載らない分まで返す — どのモデルに何 % の枠が
//! 掛かっていて、いつ開くかが分かる。トークンも消費しない。
//!
//! この口は Anthropic の OAuth にだけあり、Bedrock には無い。「無い」ことは
//! 空実装ではなく [`crate::provider::Preset`] が `None` を返すことで示す。
//!
//! 公開ドキュメントにも無い。返る形が変われば読めなくなるので、読めない
//! ものは**情報なし** (`None`) に落とし、gateway の他の動きを巻き込まない。

use serde::Deserialize;

use crate::credential::Credential;
use crate::credential::time::parse_rfc3339;
use crate::denial::{Denial, RESET_SLACK, Reason, Scope};
use crate::egress::{BoxFuture, EgressRequest, Headers};
use crate::provider::{ProbeRequest, QuotaApi};
use crate::quota::QuotaLimit;
use crate::{Error, Result};

/// 問い合わせを諦めるまで。
///
/// 利用状況の一覧は人が見て待つ画面なので、上限を短く切る。相手が黙っていても
/// 他の credential の分まで返らなくなることはない。
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 使い切ったとみなす使用率。
const EXHAUSTED: f64 = 100.0;

/// 枠ヘッダを引き出すために投げるモデル。
///
/// ヘッダを得るには実リクエストが要る (副作用ゼロで枠だけ返す口は見つかって
/// いない、DR-0007)。一番小さいモデルに `max_tokens = 1` で投げて、消費を
/// 最小にする。
const PROBE_MODEL: &str = "claude-haiku-4-5-20251001";

/// `/api/oauth/usage` で枠を聞く。
pub struct OauthUsage {
    base_url: String,
}

impl OauthUsage {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl QuotaApi for OauthUsage {
    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<Vec<QuotaLimit>>> {
        Box::pin(async move {
            fetch(http, &self.base_url, credential)
                .await
                .ok_or_else(|| {
                    Error::Config("could not fetch or parse the quota response".to_owned())
                })
        })
    }

    /// 聞けた枠を、範囲ごとの締め出し指示へ読み解く。
    ///
    /// 使い切った枠 (`percent` が 100 以上) は、開く時刻まで締め出す。
    /// 使い切っていない枠は、その範囲を開ける。**`is_active` は見ない** —
    /// 実測 (2026-08-01) では 47 % の枠が `is_active: true` を返す。あれは
    /// 「今どの枠を見ているか」であって「塞がっている」ではない。
    ///
    /// いつ開くか (`resets_at`) が分からない枠については何も言わない。期限を
    /// 決められないものを締め出すと、いつ戻すかも決められない。
    fn denials(&self, limits: &[QuotaLimit], _now: i64) -> Vec<(Scope, Option<Denial>)> {
        let mut entries = Vec::new();
        for limit in limits {
            let scope = scope_of(limit);
            if limit.percent < EXHAUSTED {
                entries.push((scope, None));
                continue;
            }
            let Some(reset) = limit.resets_at.as_deref().and_then(parse_rfc3339) else {
                continue;
            };
            entries.push((
                scope.clone(),
                Some(Denial {
                    until: reset + RESET_SLACK,
                    reason: Reason::Limited,
                    scope,
                }),
            ));
        }
        entries
    }

    /// 枠ヘッダを引き出すための最小の 1 本。
    ///
    /// 一番小さいモデルに 1 トークンだけ頼む。本文は捨てて構わないので、
    /// 中身は最短で通る形にする。OAuth の beta フラグを載せるのは、
    /// 載せないとサブスクの認証が通らないため。
    fn probe_request(&self) -> Option<ProbeRequest> {
        Some(ProbeRequest {
            model: PROBE_MODEL.to_owned(),
            request: EgressRequest {
                path: "/v1/messages".to_owned(),
                query: None,
                body: serde_json::json!({
                    "model": PROBE_MODEL,
                    "max_tokens": 1,
                    "messages": [{"role": "user", "content": "."}],
                }),
                headers: Headers::new(vec![
                    ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
                    ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
                ]),
            },
        })
    }
}

/// この credential の枠を聞く。読めなければ `None`。
async fn fetch(
    http: &reqwest::Client,
    base_url: &str,
    credential: &Credential,
) -> Option<Vec<QuotaLimit>> {
    let url = format!("{}/api/oauth/usage", base_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .header("authorization", credential.bearer())
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .timeout(TIMEOUT)
        .send()
        .await;

    let resp = match resp {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(credential = %credential.id, %e, "cannot query the quota");
            return None;
        }
    };
    let status = resp.status();
    let body = match resp.text().await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(credential = %credential.id, %e, "cannot read the quota response");
            return None;
        }
    };
    if !status.is_success() {
        tracing::warn!(
            credential = %credential.id,
            status = status.as_u16(),
            "枠を聞けませんでした"
        );
        return None;
    }
    parse(&body)
}

/// 応答を読む。知っている形でなければ `None`。
fn parse(body: &str) -> Option<Vec<QuotaLimit>> {
    /// 応答のうち、こちらが使う部分だけ。
    ///
    /// 同じ内容が `five_hour` / `seven_day` / `seven_day_opus` のような欄にも
    /// 現れるが、そちらは中身が `null` の欄が並ぶだけで、どの枠がどのモデルに
    /// 掛かるかを持たない。**`limits` を正本にする**。
    #[derive(Deserialize)]
    struct Response {
        limits: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        kind: String,
        percent: f64,
        #[serde(default)]
        severity: Option<String>,
        #[serde(default)]
        resets_at: Option<String>,
        #[serde(default)]
        scope: Option<ScopeField>,
        #[serde(default)]
        is_active: bool,
    }
    #[derive(Deserialize)]
    struct ScopeField {
        #[serde(default)]
        model: Option<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        display_name: Option<String>,
    }

    let parsed: Response = match serde_json::from_str(body) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(%e, "the quota response shape has changed");
            return None;
        }
    };
    Some(
        parsed
            .limits
            .into_iter()
            .map(|e| {
                let model = e.scope.and_then(|s| s.model);
                QuotaLimit {
                    kind: e.kind,
                    percent: e.percent,
                    severity: e.severity,
                    resets_at: e.resets_at,
                    model_id: model.as_ref().and_then(|m| m.id.clone()),
                    model: model.and_then(|m| m.display_name),
                    is_active: e.is_active,
                }
            })
            .collect(),
    )
}

/// この枠はどの範囲に効くか。
///
/// モデルが付かない枠 (`session` / `weekly_all`) は経路全体。モデルが
/// 付く枠 (`weekly_scoped`) はそのモデル群だけ。識別子が返るならそれで
/// 一致を見て、返らないなら表示名から形を起こす。
fn scope_of(limit: &QuotaLimit) -> Scope {
    if let Some(id) = &limit.model_id {
        return Scope::Model(id.clone());
    }
    match &limit.model {
        Some(name) => Scope::Models(pattern_for(name)),
        None => Scope::Everything,
    }
}

/// 表示名からモデルの形を起こす。`Fable` → `claude-fable-*`。
///
/// 名前を並べて持たない (`fable` だけ特別扱いする等をしない) のは、個別枠が
/// 増えたときに実装を直さずに済ませるため。
fn pattern_for(display_name: &str) -> String {
    let name = display_name
        .trim()
        .to_lowercase()
        .replace(char::is_whitespace, "-");
    if name.starts_with("claude") {
        format!("{name}-*")
    } else {
        format!("claude-{name}-*")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実物の応答 (2026-08-01 実測)。使わない欄も、そのまま読み飛ばせるか
    /// 確かめるために残してある。
    const SAMPLE: &str = r#"{
      "five_hour": {"utilization": 0.0, "resets_at": null, "limit_dollars": null},
      "seven_day": {"utilization": 100.0, "resets_at": "2026-08-02T08:59:59.571539+00:00"},
      "seven_day_opus": null,
      "seven_day_sonnet": null,
      "extra_usage": {"is_enabled": false, "monthly_limit": 5000},
      "limits": [
        {"kind": "session", "group": "session", "percent": 0, "severity": "normal",
         "resets_at": null, "scope": null, "is_active": false},
        {"kind": "weekly_all", "group": "weekly", "percent": 100, "severity": "critical",
         "resets_at": "2026-08-02T08:59:59.571539+00:00", "scope": null, "is_active": true},
        {"kind": "weekly_scoped", "group": "weekly", "percent": 80, "severity": "warning",
         "resets_at": "2026-08-02T08:59:59.571875+00:00",
         "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
         "is_active": false}
      ],
      "member_dashboard_available": false
    }"#;

    const NOW: i64 = 1_800_000_000;
    /// NOW の 5000 秒後。枠の開く時刻は小数秒つきで返ってくる。
    const RESET_ISO: &str = "2027-01-15T09:23:20.571539+00:00";
    const RESET: i64 = NOW + 5000;

    const FABLE: &str = "claude-fable-5";
    const HAIKU: &str = "claude-haiku-4-5-20251001";

    fn limit(kind: &str, percent: f64, model: Option<&str>, resets_at: Option<&str>) -> QuotaLimit {
        QuotaLimit {
            kind: kind.to_owned(),
            percent,
            severity: None,
            resets_at: resets_at.map(str::to_owned),
            model: model.map(str::to_owned),
            model_id: None,
            // 実測どおり、塞がっていない枠でも立つ。判断には使わない。
            is_active: true,
        }
    }

    fn api() -> OauthUsage {
        OauthUsage::new("https://api.anthropic.com")
    }

    // ---------- 枠ヘッダを引き出す 1 本 ----------

    /// 投げるのは一番小さいモデルに 1 トークンだけ。
    ///
    /// 枠を見るために枠を減らすので、消費は最小にする (DR-0007)。
    #[test]
    fn the_probe_asks_for_the_smallest_possible_answer() {
        let probe = api().probe_request().expect("最小 request がある");

        assert_eq!(probe.model, HAIKU, "一番小さいモデル");
        assert_eq!(probe.request.path, "/v1/messages");
        assert_eq!(probe.request.body["model"], HAIKU, "本文にも同じ名前");
        assert_eq!(probe.request.body["max_tokens"], 1);
        assert_eq!(
            probe.request.headers.get("anthropic-beta"),
            Some("oauth-2025-04-20"),
            "サブスクの認証はこのフラグが要る"
        );
    }

    // ---------- 応答の読み取り ----------

    #[test]
    fn reads_every_limit() {
        let limits = parse(SAMPLE).expect("知っている形");
        assert_eq!(limits.len(), 3);
        assert_eq!(limits[1].kind, "weekly_all");
        assert_eq!(limits[1].percent, 100.0);
        assert_eq!(limits[1].severity.as_deref(), Some("critical"));
        assert!(limits[1].is_active);
        assert_eq!(limits[1].model, None, "全体の枠にモデルは付かない");
    }

    /// モデル別の枠は、掛かる相手の名前を持つ。応答ヘッダには出てこない情報。
    #[test]
    fn a_scoped_limit_names_its_model() {
        let limits = parse(SAMPLE).unwrap();
        let scoped = &limits[2];
        assert_eq!(scoped.kind, "weekly_scoped");
        assert_eq!(scoped.model.as_deref(), Some("Fable"));
        assert_eq!(scoped.model_id, None, "実測では識別子は返らない");
        assert_eq!(
            scoped.resets_at.as_deref(),
            Some("2026-08-02T08:59:59.571875+00:00")
        );
    }

    /// 知らない形は情報なしに落とす。読めないものを推測で埋めない。
    #[test]
    fn an_unknown_shape_yields_nothing() {
        for body in ["", "{}", "null", r#"{"limits": "soon"}"#, "<html>"] {
            assert_eq!(parse(body), None, "{body}");
        }
    }

    /// 欄が増えても減っても、こちらが使う分が揃っていれば読める。
    #[test]
    fn unknown_fields_are_ignored() {
        let body = r#"{"limits": [{"kind": "weekly_all", "percent": 12.5,
          "is_active": false, "brand_new_field": {"nested": 1}}], "and_this_too": []}"#;
        let limits = parse(body).unwrap();
        assert_eq!(limits[0].percent, 12.5);
        assert_eq!(limits[0].severity, None);
        assert_eq!(limits[0].resets_at, None);
    }

    /// 出す JSON は upstream の語のまま。中身の無い欄は出さない。
    #[test]
    fn the_json_keeps_the_upstream_words() {
        let limits = parse(SAMPLE).unwrap();
        let json = serde_json::to_value(&limits).unwrap();
        assert_eq!(json[1]["kind"], "weekly_all");
        assert_eq!(json[2]["model"], "Fable");
        assert!(json[0].get("resets_at").is_none(), "null は出さない");
        assert!(json[0].get("model").is_none());
    }

    // ---------- 枠 → 締め出しの指示 ----------

    /// アカウント全体に掛かる枠を使い切ったら、全モデルを締め出す。
    #[test]
    fn an_exhausted_account_limit_closes_every_model() {
        let entries = api().denials(&[limit("weekly_all", 100.0, None, Some(RESET_ISO))], NOW);
        assert_eq!(
            entries,
            vec![(
                Scope::Everything,
                Some(Denial {
                    until: RESET + RESET_SLACK,
                    reason: Reason::Limited,
                    scope: Scope::Everything,
                })
            )]
        );
    }

    /// モデル別の枠を使い切ったら、そのモデル群だけを締め出す。
    #[test]
    fn an_exhausted_scoped_limit_closes_only_its_models() {
        let entries = api().denials(
            &[limit(
                "weekly_scoped",
                100.0,
                Some("Fable"),
                Some(RESET_ISO),
            )],
            NOW,
        );
        let (scope, denial) = &entries[0];
        assert_eq!(*scope, Scope::Models("claude-fable-*".to_owned()));
        assert_eq!(denial.as_ref().unwrap().until, RESET + RESET_SLACK);
        assert!(scope.covers(FABLE));
        assert!(!scope.covers(HAIKU), "他のモデルは巻き込まない");
    }

    /// 使い切っていない枠は、その範囲を開ける指示になる。
    #[test]
    fn a_limit_below_the_line_reopens_its_scope() {
        let entries = api().denials(
            &[
                limit("weekly_all", 99.9, None, Some(RESET_ISO)),
                limit("weekly_scoped", 0.0, Some("Fable"), Some(RESET_ISO)),
            ],
            NOW,
        );
        assert_eq!(
            entries,
            vec![
                (Scope::Everything, None),
                (Scope::Models("claude-fable-*".to_owned()), None),
            ]
        );
    }

    /// いつ開くか分からない枠については何も言わない。戻す時刻を決められない。
    #[test]
    fn an_exhausted_limit_without_a_reset_is_not_used() {
        assert!(
            api()
                .denials(&[limit("session", 100.0, None, None)], NOW)
                .is_empty()
        );
    }

    /// 表示名からモデルの形を起こす。名前を並べて持たない。
    #[test]
    fn a_display_name_becomes_a_model_pattern() {
        assert_eq!(pattern_for("Fable"), "claude-fable-*");
        assert_eq!(pattern_for(" Opus "), "claude-opus-*");
        assert_eq!(pattern_for("Claude Sonnet"), "claude-sonnet-*");
        assert!(Scope::Models(pattern_for("Fable")).covers(FABLE));
        assert!(!Scope::Models(pattern_for("Fable")).covers(HAIKU));
    }

    /// 識別子が返るなら、それで一致を見る (今の実測では返らない)。
    #[test]
    fn an_identifier_is_matched_exactly() {
        let mut with_id = limit("weekly_scoped", 100.0, Some("Fable"), Some(RESET_ISO));
        with_id.model_id = Some(FABLE.to_owned());

        let entries = api().denials(&[with_id], NOW);
        assert_eq!(entries[0].0, Scope::Model(FABLE.to_owned()));
        assert!(!entries[0].0.covers("claude-fable-6"), "完全一致で見る");
    }

    // ---------- 通信 ----------

    /// 接続先は preset を組むときに決まる (Wire と同じ根を使う)。
    #[test]
    fn asks_the_same_host_as_the_wire() {
        assert_eq!(api().base_url, "https://api.anthropic.com");
    }

    /// 届かなければ失敗を返す。optional なのは capability の有無であり、
    /// capability を呼んだ後の通信失敗まで成功扱いにはしない。
    #[tokio::test]
    async fn an_unreachable_host_is_an_error() {
        // 誰も待ち受けていない先。接続が拒まれた時点で戻る。
        let api = OauthUsage::new("http://127.0.0.1:1");
        let result = api
            .fetch(&reqwest::Client::new(), &Credential::for_test("tok"))
            .await;

        assert!(result.is_err());
    }
}
