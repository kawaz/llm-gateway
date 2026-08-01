//! サブスクの枠を、専用の口から直接聞く (DR-0007)。
//!
//! 転送の応答ヘッダから読める枠は 5 時間 / 7 日の 2 つだけで、**モデル別の枠は
//! 載らない**。`/api/oauth/usage` は載らない分まで返す — どのモデルに何 % の枠が
//! 掛かっていて、いつ開くかが分かる。トークンも消費しない。
//!
//! この口は公開ドキュメントに無い。返る形が変われば読めなくなるので、読めない
//! ものは**情報なし** (`None`) に落とし、gateway の他の動きを巻き込まない。

use serde::{Deserialize, Serialize};

use crate::credential::Credential;

/// 問い合わせを諦めるまで。
///
/// 利用状況の一覧は人が見て待つ画面なので、上限を短く切る。相手が黙っていても
/// 他の credential の分まで返らなくなることはない。
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 枠 1 本。
///
/// 語は upstream のものをそのまま使う (`session` / `weekly_all` /
/// `weekly_scoped`)。こちらで言い換えると、実物と照らし合わせる人が
/// 対応表を覚えることになる。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    /// `session` / `weekly_all` / `weekly_scoped` など。
    pub kind: String,
    /// 使用率 (0〜100)。
    pub percent: f64,
    /// `normal` / `warning` / `critical` など。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    /// この枠が開く時刻 (RFC 3339)。開く予定が無い枠では欠ける。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// 枠が掛かるモデルの表示名 (`Fable` など)。`weekly_scoped` だけが持つ。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 同じモデルの識別子。実測 (2026-08-01) では常に欠ける。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// upstream が「今この枠を見ている」と言っている印。
    ///
    /// **塞がっているという意味ではない** — 実測 (2026-08-01): 47 % で
    /// `severity: normal` の枠が `is_active: true` を返す。
    pub is_active: bool,
}

/// この credential の枠を聞く。読めなければ `None`。
pub async fn fetch(
    http: &reqwest::Client,
    base_url: &str,
    credential: &Credential,
) -> Option<Vec<Limit>> {
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
            tracing::warn!(credential = %credential.id, %e, "枠を聞きに行けません");
            return None;
        }
    };
    let status = resp.status();
    let body = match resp.text().await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(credential = %credential.id, %e, "枠の応答を読めません");
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
pub fn parse(body: &str) -> Option<Vec<Limit>> {
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
        scope: Option<Scope>,
        #[serde(default)]
        is_active: bool,
    }
    #[derive(Deserialize)]
    struct Scope {
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
            tracing::warn!(%e, "枠の形が変わっています");
            return None;
        }
    };
    Some(
        parsed
            .limits
            .into_iter()
            .map(|e| {
                let model = e.scope.and_then(|s| s.model);
                Limit {
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
}
