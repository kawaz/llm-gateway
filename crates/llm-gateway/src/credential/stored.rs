//! 保存される認証情報の形。
//!
//! 2 層に分ける。**トップ**は人が触ってよい層 — 運用の設定 (`priority` /
//! `disabled` / `excluded_models`) と、gateway が観測して書き戻した値
//! (`last_refresh` / `denied_beta`)。**payload** はプロバイダが返した認証
//! 情報そのままで、人が手で直すものではない。平坦に混ぜると、どれを直して
//! よいのか読み手に区別が付かない。
//!
//! payload は `type` で判別する enum。「account_id は ChatGPT 経路だけが
//! 要求する」「Bedrock は API キー認証なので refresh token が無い」といった
//! 暗黙の約束を型で表す。平坦な 1 つの構造体に押し込むと、使わない項目を
//! 空文字で埋めることになり、読み手も実装も「入っていないのが正しい」のか
//! 「入れ忘れ」なのか区別できない。

use std::collections::{BTreeMap, BTreeSet};

use serde::ser::SerializeMap as _;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use zeroize::Zeroize;

use super::time::{format_rfc3339, parse_rfc3339};

/// login で認可を通せる種別。
///
/// 設定ファイルの `type` と保存ファイルの `type` は同じ語を使う。
/// API キー認証 (`bedrock_api_key`) は認可の流れを持たないので、ここには無い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Anthropic のサブスク OAuth。
    Claude,
    /// ChatGPT のサブスク OAuth。
    Codex,
}

impl Kind {
    /// `type` の語から引く。login を要さない種別 (bedrock / relay) は `None`。
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

/// OAuth で受け取った認証情報。
///
/// `access_token` / `refresh_token` は [`Drop`] で消す。ファイルから読んだ
/// 時点でメモリに乗るのは避けられないので、せめて残さない。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OauthTokens {
    pub access_token: String,
    pub refresh_token: String,

    /// access token の期限。RFC 3339。
    #[serde(default)]
    pub expired: String,

    /// プロバイダが名乗ったアカウント。どの人格の認証情報か見分けるために持つ。
    #[serde(default)]
    pub email: String,

    /// プロバイダが返したが gateway は解釈しない項目。読んだまま書き戻す。
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Drop for OauthTokens {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

/// ChatGPT の OAuth。
///
/// `account_id` は ChatGPT 経路だけが要求する。id_token の claim から拾うので、
/// 返らないこともある。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexTokens {
    #[serde(flatten)]
    pub oauth: OauthTokens,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl Drop for CodexTokens {
    fn drop(&mut self) {
        if let Some(id_token) = &mut self.id_token {
            id_token.zeroize();
        }
    }
}

/// API キー認証 (Bedrock)。
///
/// 更新の口が無いので refresh token を持たない。期限が切れたらキーを
/// 発行し直す以外に手が無く、gateway 側で回復できない。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApiKey {
    pub api_key: String,

    /// キーの期限。RFC 3339。期限を持たないキーもあるので省略できる。
    #[serde(default)]
    pub expired: String,

    /// プロバイダが返したが gateway は解釈しない項目。読んだまま書き戻す。
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Drop for ApiKey {
    fn drop(&mut self) {
        self.api_key.zeroize();
    }
}

/// プロバイダから受け取った認証情報。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Payload {
    ClaudeOauth(OauthTokens),
    CodexOauth(CodexTokens),
    BedrockApiKey(ApiKey),
}

impl Payload {
    /// 保存ファイルに書く `type`。
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ClaudeOauth(_) => "claude_oauth",
            Self::CodexOauth(_) => "codex_oauth",
            Self::BedrockApiKey(_) => "bedrock_api_key",
        }
    }

    /// 更新に使える OAuth の種別。API キー認証は `None`。
    pub fn oauth_kind(&self) -> Option<Kind> {
        match self {
            Self::ClaudeOauth(_) => Some(Kind::Claude),
            Self::CodexOauth(_) => Some(Kind::Codex),
            Self::BedrockApiKey(_) => None,
        }
    }

    /// upstream に載せる秘密。方式 (Bearer / x-api-key) は provider が決める。
    pub fn secret(&self) -> &str {
        match self {
            Self::ClaudeOauth(t) => &t.access_token,
            Self::CodexOauth(c) => &c.oauth.access_token,
            Self::BedrockApiKey(k) => &k.api_key,
        }
    }

    /// 秘密の期限。RFC 3339。
    pub fn expired(&self) -> &str {
        match self {
            Self::ClaudeOauth(t) => &t.expired,
            Self::CodexOauth(c) => &c.oauth.expired,
            Self::BedrockApiKey(k) => &k.expired,
        }
    }

    /// 更新した token を書き戻す先。API キー認証は `None`。
    pub fn oauth_tokens_mut(&mut self) -> Option<&mut OauthTokens> {
        match self {
            Self::ClaudeOauth(t) => Some(t),
            Self::CodexOauth(c) => Some(&mut c.oauth),
            Self::BedrockApiKey(_) => None,
        }
    }

    /// 更新に使う refresh token。持たない種別は `None`。
    pub fn refresh_token(&self) -> Option<&str> {
        match self {
            Self::ClaudeOauth(t) => Some(&t.refresh_token),
            Self::CodexOauth(c) => Some(&c.oauth.refresh_token),
            Self::BedrockApiKey(_) => None,
        }
    }

    /// プロバイダが名乗ったアカウント。分からなければ空。
    pub fn email(&self) -> &str {
        match self {
            Self::ClaudeOauth(t) => &t.email,
            Self::CodexOauth(c) => &c.oauth.email,
            Self::BedrockApiKey(_) => "",
        }
    }

    /// ChatGPT 経路が要求するアカウント識別子。
    pub fn account_id(&self) -> Option<&str> {
        match self {
            Self::CodexOauth(c) => c.account_id.as_deref(),
            Self::ClaudeOauth(_) | Self::BedrockApiKey(_) => None,
        }
    }
}

/// ディスク上の認証情報。
///
/// serde の派生では `type` と `payload` が隣り合ってしまう (adjacently tagged
/// enum を flatten するため) ので、書き出しだけ手で並べる。読み手が先に見たい
/// のは触ってよい層で、payload は最後でよい。
///
/// Design rationale: 書き出しを手で並べる代わり、読み込みは派生のままにして
/// ある。両方手で書くと二重管理になるので、往復とキーの並びを試験で押さえる。
#[derive(Debug, Clone, Deserialize)]
pub struct StoredCredential {
    /// 小さいほど先に選ばれる。
    #[serde(default)]
    pub priority: i32,

    /// true の間は選択対象から外す。
    #[serde(default)]
    pub disabled: bool,

    /// この認証情報で扱わないモデル。`claude-opus-*` のような後方ワイルドカードが書ける。
    #[serde(default)]
    pub excluded_models: Vec<String>,

    /// 最後に更新した時刻。RFC 3339。
    #[serde(default)]
    pub last_refresh: String,

    /// 拒否された beta フラグを再び試すまでの間隔 (ミリ秒)。
    #[serde(default = "default_denied_beta_expires_ms")]
    pub denied_beta_expires_ms: u64,

    /// upstream が拒否した beta フラグ → 最後に確認した時刻 (RFC 3339)。
    ///
    /// 期限切れの記録は消さずに残す。「試してよい」状態として扱うので害は
    /// 無く、消しに行くと成功のたびに書き込みが要る (DR-0003)。
    #[serde(default)]
    pub denied_beta: BTreeMap<String, String>,

    /// プロバイダから受け取った認証情報。
    #[serde(flatten)]
    pub payload: Payload,
}

/// 24 時間。upstream が対応したときに、その日のうちには拾い直せる長さ。
fn default_denied_beta_expires_ms() -> u64 {
    86_400_000
}

impl Serialize for StoredCredential {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", self.payload.type_name())?;
        map.serialize_entry("priority", &self.priority)?;
        map.serialize_entry("disabled", &self.disabled)?;
        map.serialize_entry("excluded_models", &self.excluded_models)?;
        map.serialize_entry("last_refresh", &self.last_refresh)?;
        map.serialize_entry("denied_beta_expires_ms", &self.denied_beta_expires_ms)?;
        map.serialize_entry("denied_beta", &self.denied_beta)?;
        match &self.payload {
            Payload::ClaudeOauth(p) => map.serialize_entry("payload", p)?,
            Payload::CodexOauth(p) => map.serialize_entry("payload", p)?,
            Payload::BedrockApiKey(p) => map.serialize_entry("payload", p)?,
        }
        map.end()
    }
}

impl StoredCredential {
    /// 運用の設定を既定にして作る。初めての login で使う。
    pub fn new(payload: Payload) -> Self {
        Self {
            priority: 0,
            disabled: false,
            excluded_models: Vec::new(),
            last_refresh: String::new(),
            denied_beta_expires_ms: default_denied_beta_expires_ms(),
            denied_beta: BTreeMap::new(),
            payload,
        }
    }

    /// このモデルをこの認証情報で扱ってよいか。
    pub fn accepts_model(&self, model: &str) -> bool {
        !self
            .excluded_models
            .iter()
            .any(|pat| matches_pattern(pat, model))
    }

    /// この beta フラグを今回は落とすか (DR-0003)。
    ///
    /// 記録が無ければ通す。あっても `denied_beta_expires_ms` を過ぎていれば
    /// 通してみる — upstream が対応していれば、そのまま使えるようになる。
    /// 時刻が読めない記録も通す側に倒す。「いつ試したか分からない」ものを
    /// 落とし続けると、書き損じ 1 つで機能が永久に消える。
    pub fn denies_beta(&self, flag: &str, now_unix: i64) -> bool {
        let Some(seen) = self.denied_beta.get(flag).map(String::as_str) else {
            return false;
        };
        let Some(seen) = parse_rfc3339(seen) else {
            return false;
        };
        let elapsed_ms = now_unix.saturating_sub(seen).saturating_mul(1000);
        elapsed_ms < i64::try_from(self.denied_beta_expires_ms).unwrap_or(i64::MAX)
    }

    /// 今この時点で落とす beta フラグ。
    pub fn denied_beta_at(&self, now_unix: i64) -> BTreeSet<String> {
        self.denied_beta
            .keys()
            .filter(|flag| self.denies_beta(flag, now_unix))
            .cloned()
            .collect()
    }

    /// 拒否されたフラグと、確認した時刻を覚える。
    pub fn record_denied_beta(&mut self, flags: &[String], now_unix: i64) {
        let now = format_rfc3339(now_unix);
        for flag in flags {
            self.denied_beta.insert(flag.clone(), now.clone());
        }
    }
}

/// 除外パターン。`*` は 1 個まで、前方・後方・両端のいずれかに置く。
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

    /// 2026-07-27T19:00:00Z
    const NOW: i64 = 1_785_178_800;
    const DAY_MS: u64 = 86_400_000;

    fn oauth(access_token: &str) -> OauthTokens {
        oauth_with(access_token, BTreeMap::new())
    }

    fn oauth_with(access_token: &str, extra: BTreeMap<String, Value>) -> OauthTokens {
        OauthTokens {
            access_token: access_token.into(),
            refresh_token: "rt".into(),
            expired: "2026-07-28T02:54:00+09:00".into(),
            email: "someone@example.com".into(),
            extra,
        }
    }

    fn api_key(extra: BTreeMap<String, Value>) -> ApiKey {
        ApiKey {
            api_key: "ak".into(),
            expired: "2026-08-02T10:08:18+09:00".into(),
            extra,
        }
    }

    fn sample(excluded: &[&str]) -> StoredCredential {
        let mut c = StoredCredential::new(Payload::ClaudeOauth(oauth("at")));
        c.priority = 10;
        c.excluded_models = excluded.iter().map(|s| (*s).to_owned()).collect();
        c
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

    /// 実運用の設定で使われている形 (`claude-opus-*` など)。
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

    /// 実運用の除外リスト (fable 専用アカウント)。
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

    /// 保存する形。触ってよい層が先、payload が最後。
    #[test]
    fn writes_the_documented_shape() {
        let mut c = sample(&["claude-opus-*"]);
        c.last_refresh = "2026-07-27T18:54:00+09:00".into();
        c.record_denied_beta(&["advisor-tool-2026-03-01".to_owned()], NOW);

        let json = serde_json::to_string_pretty(&c).unwrap();
        let written: serde_json::Map<String, Value> = serde_json::from_str(&json).unwrap();
        let keys: Vec<&str> = written.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "type",
                "priority",
                "disabled",
                "excluded_models",
                "last_refresh",
                "denied_beta_expires_ms",
                "denied_beta",
                "payload",
            ],
            "並びごと固定する:\n{json}"
        );

        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "claude_oauth");
        assert_eq!(v["excluded_models"][0], "claude-opus-*", "ハイフンでない");
        assert_eq!(v["denied_beta_expires_ms"], 86_400_000);
        assert_eq!(
            v["denied_beta"]["advisor-tool-2026-03-01"],
            format_rfc3339(NOW)
        );
        assert_eq!(v["payload"]["access_token"], "at");
        assert_eq!(v["payload"]["email"], "someone@example.com");
        assert!(
            v["payload"].get("account_id").is_none(),
            "claude 側は account_id を持たない: {json}"
        );
    }

    /// 書いたものを読み直して同じになる (往復で欠けない)。
    #[test]
    fn round_trips_every_variant() {
        let mut extra = BTreeMap::new();
        extra.insert("id_token".to_owned(), Value::String("jwt".to_owned()));

        let payloads = [
            Payload::ClaudeOauth(oauth_with("at", extra.clone())),
            Payload::CodexOauth(CodexTokens {
                oauth: oauth("at"),
                id_token: None,
                account_id: Some("acc-1".to_owned()),
            }),
            Payload::BedrockApiKey(api_key(extra)),
        ];

        for payload in payloads {
            let mut before = StoredCredential::new(payload);
            before.priority = 7;
            before.disabled = true;
            before.excluded_models = vec!["claude-haiku-*".to_owned()];
            before.last_refresh = "2026-07-27T18:54:00+09:00".into();
            before.denied_beta_expires_ms = 3_600_000;
            before.record_denied_beta(&["oauth-2025-04-20".to_owned()], NOW);

            let json = serde_json::to_string(&before).unwrap();
            let after: StoredCredential = serde_json::from_str(&json).unwrap();

            assert_eq!(serde_json::to_string(&after).unwrap(), json, "{json}");
            assert_eq!(after.payload.type_name(), before.payload.type_name());
            assert_eq!(after.payload.secret(), before.payload.secret());
            assert_eq!(after.payload.account_id(), before.payload.account_id());
            assert_eq!(after.denied_beta_expires_ms, 3_600_000);
        }
    }

    /// 種別ごとに持てる項目が違う。
    #[test]
    fn payload_shape_differs_by_type() {
        let claude = Payload::ClaudeOauth(oauth("at"));
        assert_eq!(claude.oauth_kind(), Some(Kind::Claude));
        assert_eq!(claude.refresh_token(), Some("rt"));
        assert_eq!(claude.account_id(), None, "claude に account_id は無い");

        let codex = Payload::CodexOauth(CodexTokens {
            oauth: oauth("at"),
            id_token: None,
            account_id: Some("acc-1".to_owned()),
        });
        assert_eq!(codex.oauth_kind(), Some(Kind::Codex));
        assert_eq!(codex.account_id(), Some("acc-1"));

        let bedrock = Payload::BedrockApiKey(api_key(BTreeMap::new()));
        assert_eq!(bedrock.oauth_kind(), None, "更新の口が無い");
        assert_eq!(
            bedrock.refresh_token(),
            None,
            "OAuth ではないので refresh token を持たない"
        );
        assert_eq!(bedrock.secret(), "ak");
        assert_eq!(bedrock.email(), "");
    }

    /// 書き出す `type` は設定ファイルの語と同じ。
    ///
    /// 保存側と設定側で語が違うと、`login` の案内文をそのまま config.toml に
    /// 貼れなくなる。
    #[test]
    fn type_name_matches_config_type() {
        let claude = Payload::ClaudeOauth(oauth("at"));
        assert_eq!(claude.type_name(), Kind::Claude.config_type());

        let codex = Payload::CodexOauth(CodexTokens {
            oauth: oauth("at"),
            id_token: None,
            account_id: None,
        });
        assert_eq!(codex.type_name(), Kind::Codex.config_type());

        let bedrock = Payload::BedrockApiKey(api_key(BTreeMap::new()));
        assert_eq!(bedrock.type_name(), "bedrock_api_key");
    }

    /// 運用の設定は省略できる。payload は省略できない。
    #[test]
    fn operational_fields_may_be_absent() {
        let raw = r#"{
            "type": "claude_oauth",
            "payload": {"access_token": "at", "refresh_token": "rt"}
        }"#;
        let c: StoredCredential = serde_json::from_str(raw).unwrap();
        assert_eq!(c.priority, 0);
        assert!(!c.disabled);
        assert!(c.excluded_models.is_empty());
        assert!(c.denied_beta.is_empty());
        assert_eq!(c.denied_beta_expires_ms, 86_400_000, "既定は 24 時間");

        let err = serde_json::from_str::<StoredCredential>(r#"{"type": "claude_oauth"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("payload"), "{err}");
    }

    /// 旧形式 (平坦・cpa 互換) は読まない。
    ///
    /// 読めてしまうと、どちらの形で保存されているのか分からないまま
    /// 両方を持ち回ることになる。login で取り直せるので断る (GW-Q3)。
    #[test]
    fn flat_legacy_format_is_refused() {
        let raw = r#"{
            "type": "claude",
            "email": "someone@example.com",
            "access_token": "at",
            "refresh_token": "rt",
            "expired": "2026-07-28T02:54:00+09:00",
            "excluded-models": ["claude-opus-*"]
        }"#;
        let err = serde_json::from_str::<StoredCredential>(raw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("claude_oauth"), "指定できる語を示す: {err}");
    }

    fn with_denied(flag: &str, at: i64, expires_ms: u64) -> StoredCredential {
        let mut c = StoredCredential::new(Payload::ClaudeOauth(oauth("at")));
        c.denied_beta_expires_ms = expires_ms;
        c.record_denied_beta(&[flag.to_owned()], at);
        c
    }

    /// 記録が無いフラグは通す。
    #[test]
    fn unknown_flag_is_sent() {
        let c = with_denied("advisor-tool-2026-03-01", NOW, DAY_MS);
        assert!(!c.denies_beta("context-management-2025-06-27", NOW));
    }

    /// 期限内の記録は落とす。
    #[test]
    fn recently_denied_flag_is_dropped() {
        let c = with_denied("advisor-tool-2026-03-01", NOW, DAY_MS);
        assert!(c.denies_beta("advisor-tool-2026-03-01", NOW));
        assert!(c.denies_beta("advisor-tool-2026-03-01", NOW + 3600));
    }

    /// 期限が切れたら試す (upstream が対応していれば戻る)。
    #[test]
    fn expired_denial_is_retried() {
        let c = with_denied("advisor-tool-2026-03-01", NOW, DAY_MS);
        assert!(
            c.denies_beta("advisor-tool-2026-03-01", NOW + 86_399),
            "24 時間未満はまだ落とす"
        );
        assert!(
            !c.denies_beta("advisor-tool-2026-03-01", NOW + 86_400),
            "24 時間経ったら試す"
        );
        assert!(!c.denies_beta("advisor-tool-2026-03-01", NOW + 86_400 * 30));
    }

    /// 間隔は credential ごとに変えられる。
    #[test]
    fn interval_is_configurable() {
        let c = with_denied("f", NOW, 3_600_000);
        assert!(c.denies_beta("f", NOW + 3599));
        assert!(!c.denies_beta("f", NOW + 3600), "1 時間で試し直す");

        let never = with_denied("f", NOW, 0);
        assert!(!never.denies_beta("f", NOW), "0 なら毎回試す");
    }

    /// 時刻が読めない記録は落とさない (書き損じで機能を永久に失わない)。
    #[test]
    fn unreadable_timestamp_is_retried() {
        let mut c = StoredCredential::new(Payload::ClaudeOauth(oauth("at")));
        c.denied_beta.insert("f".to_owned(), "きのう".to_owned());
        assert!(!c.denies_beta("f", NOW));
    }

    /// 落とす一覧は、期限内のものだけになる。
    #[test]
    fn denied_set_holds_only_live_records() {
        let mut c = StoredCredential::new(Payload::ClaudeOauth(oauth("at")));
        c.record_denied_beta(&["old".to_owned()], NOW - 86_400 * 2);
        c.record_denied_beta(&["fresh".to_owned()], NOW);

        let denied = c.denied_beta_at(NOW);
        assert_eq!(denied, BTreeSet::from(["fresh".to_owned()]));
        assert!(
            c.denied_beta.contains_key("old"),
            "期限切れの記録自体は消さない"
        );
    }

    /// 同じフラグを再び拒否されたら、時刻だけ進む。
    #[test]
    fn re_denial_updates_the_timestamp() {
        let mut c = with_denied("f", NOW, DAY_MS);
        c.record_denied_beta(&["f".to_owned()], NOW + 86_400 * 3);

        assert_eq!(c.denied_beta.len(), 1, "重複して増えない");
        assert!(c.denies_beta("f", NOW + 86_400 * 3));
    }

    /// 設定側の語から種別を引ける。login できない種別は弾く。
    #[test]
    fn kind_from_config_type() {
        assert_eq!(Kind::from_config_type("claude_oauth"), Some(Kind::Claude));
        assert_eq!(Kind::from_config_type("codex_oauth"), Some(Kind::Codex));
        for other in ["bedrock_api_key", "relay", "claude", "codex", ""] {
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
}
