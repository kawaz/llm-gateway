//! 保存される認証情報の形。
//!
//! cpa の auth JSON と同じ項目を持つ。移行期に cpa と同じファイルを
//! 読み書きできるようにしておくと、片方ずつ切り替えて様子を見られる。
//! 知らない項目は捨てずに持ち回る (cpa が書いた値を消してしまわないため)。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroize;

/// 認証情報の種別。cpa の `type` フィールドに対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Anthropic のサブスク OAuth。
    Claude,
    /// ChatGPT のサブスク OAuth。
    Codex,
}

impl Kind {
    /// 設定ファイルの `type` から引く。
    ///
    /// 保存側 (`claude` / `codex`) と設定側 (`claude_oauth` / `codex_oauth`) で
    /// 語が違う。使う側に 2 通り覚えさせないよう、CLI の `--type` も設定側の語で
    /// 受け、ここで対応づける。login を要さない種別 (bedrock / relay) は `None`。
    pub fn from_config_type(name: &str) -> Option<Self> {
        match name {
            "claude_oauth" => Some(Self::Claude),
            "codex_oauth" => Some(Self::Codex),
            _ => None,
        }
    }

    /// 設定ファイルに書くときの `type`。
    pub fn config_type(self) -> &'static str {
        match self {
            Self::Claude => "claude_oauth",
            Self::Codex => "codex_oauth",
        }
    }
}

/// ディスク上の認証情報。
///
/// `access_token` / `refresh_token` は [`Drop`] で消す。ファイルから読んだ
/// 時点でメモリに乗るのは避けられないので、せめて残さない。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredential {
    #[serde(rename = "type")]
    pub kind: Kind,

    pub email: String,

    pub access_token: String,
    pub refresh_token: String,

    /// access token の期限。RFC 3339。
    pub expired: String,

    /// 最後に更新した時刻。RFC 3339。
    #[serde(default)]
    pub last_refresh: String,

    /// 小さいほど先に選ばれる。
    #[serde(default)]
    pub priority: i32,

    /// true の間は選択対象から外す。
    #[serde(default)]
    pub disabled: bool,

    /// この認証情報で扱わないモデル。`claude-opus-*` のような後方ワイルドカードが書ける。
    #[serde(default, rename = "excluded-models")]
    pub excluded_models: Vec<String>,

    /// ChatGPT 経路が要求するアカウント識別子。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// cpa が書くが gateway は解釈しない項目。読んだまま書き戻す。
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl StoredCredential {
    /// このモデルをこの認証情報で扱ってよいか。
    pub fn accepts_model(&self, model: &str) -> bool {
        !self
            .excluded_models
            .iter()
            .any(|pat| matches_pattern(pat, model))
    }
}

impl Drop for StoredCredential {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

/// cpa の除外パターン。`*` は 1 個まで、前方・後方・両端のいずれかに置く。
fn matches_pattern(pattern: &str, model: &str) -> bool {
    match (pattern.strip_prefix('*'), pattern.strip_suffix('*')) {
        (Some(_), Some(_)) => {
            let inner = &pattern[1..pattern.len() - 1];
            inner.is_empty() || model.contains(inner)
        }
        (Some(suffix), None) => model.ends_with(suffix),
        (None, Some(prefix)) => model.starts_with(prefix),
        (None, None) => pattern == model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(excluded: &[&str]) -> StoredCredential {
        StoredCredential {
            kind: Kind::Claude,
            email: "someone@example.com".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expired: "2026-07-28T02:54:00+09:00".into(),
            last_refresh: "2026-07-27T18:54:00+09:00".into(),
            priority: 10,
            disabled: false,
            excluded_models: excluded.iter().map(|s| (*s).to_owned()).collect(),
            account_id: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn no_exclusions_accepts_everything() {
        assert!(sample(&[]).accepts_model("claude-opus-5"));
    }

    #[test]
    fn exact_name_is_excluded() {
        let c = sample(&["claude-opus-5"]);
        assert!(!c.accepts_model("claude-opus-5"));
        assert!(c.accepts_model("claude-sonnet-5"));
    }

    /// cpa の設定で実際に使われている形 (`claude-opus-*` など)。
    #[test]
    fn trailing_wildcard() {
        let c = sample(&["claude-opus-*"]);
        assert!(!c.accepts_model("claude-opus-5"));
        assert!(!c.accepts_model("claude-opus-4-8"));
        assert!(c.accepts_model("claude-sonnet-5"));
    }

    #[test]
    fn leading_wildcard() {
        let c = sample(&["*-preview"]);
        assert!(!c.accepts_model("claude-opus-5-preview"));
        assert!(c.accepts_model("claude-opus-5"));
    }

    #[test]
    fn surrounding_wildcards() {
        let c = sample(&["*haiku*"]);
        assert!(!c.accepts_model("claude-haiku-4-5-20251001"));
        assert!(c.accepts_model("claude-opus-5"));
    }

    /// 実運用の auth JSON にある形 (fable 専用アカウントの除外リスト)。
    #[test]
    fn realistic_exclusion_set() {
        let c = sample(&[
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-haiku-4-5-20251001",
            "claude-opus-*",
            "claude-sonnet-*",
            "claude-haiku-*",
        ]);
        assert!(c.accepts_model("claude-fable-5"));
        assert!(!c.accepts_model("claude-opus-5"));
        assert!(!c.accepts_model("claude-sonnet-5"));
        assert!(!c.accepts_model("claude-haiku-4-5-20251001"));
    }

    /// cpa 形式をそのまま読める。知らない項目は保持する。
    #[test]
    fn reads_cpa_json_and_keeps_unknown_fields() {
        let raw = r#"{
            "type": "claude",
            "email": "someone@example.com",
            "access_token": "at",
            "refresh_token": "rt",
            "expired": "2026-07-28T02:54:00+09:00",
            "last_refresh": "2026-07-27T18:54:00+09:00",
            "priority": 10,
            "disabled": false,
            "id_token": ""
        }"#;
        let c: StoredCredential = serde_json::from_str(raw).unwrap();
        assert_eq!(c.kind, Kind::Claude);
        assert_eq!(c.priority, 10);
        assert!(c.extra.contains_key("id_token"), "未知の項目を捨てない");

        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back.get("id_token").and_then(Value::as_str), Some(""));
        assert_eq!(back.get("type").and_then(Value::as_str), Some("claude"));
    }

    /// codex 形式は account_id を持つ。
    #[test]
    fn reads_codex_json() {
        let raw = r#"{
            "type": "codex",
            "email": "someone@example.com",
            "access_token": "at",
            "refresh_token": "rt",
            "account_id": "acc-1",
            "expired": "2026-08-02T10:08:18+09:00"
        }"#;
        let c: StoredCredential = serde_json::from_str(raw).unwrap();
        assert_eq!(c.kind, Kind::Codex);
        assert_eq!(c.account_id.as_deref(), Some("acc-1"));
    }

    /// 設定側の語から種別を引ける。login できない種別は弾く。
    #[test]
    fn kind_from_config_type() {
        assert_eq!(Kind::from_config_type("claude_oauth"), Some(Kind::Claude));
        assert_eq!(Kind::from_config_type("codex_oauth"), Some(Kind::Codex));
        for other in ["claude_bedrock", "relay", "claude", "codex", ""] {
            assert_eq!(Kind::from_config_type(other), None, "{other}");
        }
    }

    /// 引いた語で書き戻せる (help や案内文がそのまま設定に貼れる)。
    #[test]
    fn config_type_round_trips() {
        for kind in [Kind::Claude, Kind::Codex] {
            assert_eq!(Kind::from_config_type(kind.config_type()), Some(kind));
        }
    }

    /// 省略可能な項目が無くても読める。
    #[test]
    fn optional_fields_may_be_absent() {
        let raw = r#"{
            "type": "claude",
            "email": "a@b.c",
            "access_token": "at",
            "refresh_token": "rt",
            "expired": "2026-07-28T02:54:00+09:00"
        }"#;
        let c: StoredCredential = serde_json::from_str(raw).unwrap();
        assert_eq!(c.priority, 0);
        assert!(!c.disabled);
        assert!(c.excluded_models.is_empty());
    }
}
