//! 会話の見分け方。入口 (ingress) でクライアントの言い分から起こす。
//!
//! 2 つある。経路を貼り続けるための [`SessionKey`] ([`derive`]) と、
//! クライアントが自分で名乗った会話の id ([`declared_id`])。前者は名乗らない
//! 相手にも必ず付く導出値で、後者は名乗った相手にしか無い。知らせ (DR-0012) に
//! 載せるのは後者 — こちらが本文から起こした鍵を外へ出しても、受け取った側は
//! 自分の会話と突き合わせられない。
//!
//! どちらもクライアント方言の知識なので、ここに集めてある。
//!
//! 導出規則は cpa (`sdk/cliproxy/auth/selector.go`) の挙動に合わせてある。
//! 移行期に cpa と併存させたとき、両者が同じ会話を同じキーとみなすため。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};

use serde_json::Value;

/// クライアントが会話の id を載せてくるリクエストヘッダ。
///
/// Claude Code が付ける。付けない相手 (curl 等) もいるので、無ければ
/// 会話を特定しない扱いにする。
pub const SESSION_HEADER: &str = "x-claude-code-session-id";

/// クライアントが名乗った会話の id。名乗っていなければ `None`。
///
/// [`derive`] が作る [`SessionKey`] とは別物。あちらは経路を貼り続けるために
/// こちらで起こす鍵で、こちらはクライアント自身の呼び名。
pub fn declared_id(headers: &[(String, String)]) -> Option<String> {
    header(headers, SESSION_HEADER).map(str::to_owned)
}

/// session affinity のキー。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// リクエストから session キーを導出する。
///
/// Claude Code は `metadata.user_id` に会話の識別子を載せてくる。それが無い
/// クライアント (curl 等) 向けに、会話の冒頭部分のハッシュへ落ちる。
///
/// `headers` は `(名前, 値)` の並び。名前は大文字小文字を区別せず引く。
pub fn derive(body: &Value, headers: &[(String, String)]) -> SessionKey {
    if let Some(id) = from_metadata_user_id(body) {
        return SessionKey(format!("claude:{id}"));
    }
    if let Some(v) = header(headers, "x-session-id") {
        return SessionKey(format!("header:{v}"));
    }
    // Codex CLI 系が送る。`Session-Id` / `Session_id` の両表記がありうる。
    if let Some(v) = header(headers, "session-id").or_else(|| header(headers, "session_id")) {
        return SessionKey(format!("codex:{v}"));
    }
    if let Some(v) = header(headers, "x-client-request-id") {
        return SessionKey(format!("clientreq:{v}"));
    }
    if let Some(v) = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        && !v.is_empty()
    {
        return SessionKey(format!("user:{v}"));
    }
    if let Some(v) = body.get("conversation_id").and_then(Value::as_str)
        && !v.is_empty()
    {
        return SessionKey(format!("conv:{v}"));
    }
    SessionKey(message_hash(body))
}

/// `metadata.user_id` から会話 ID を取り出す。
///
/// Claude Code が送る形は 2 通り観測されている:
/// - JSON: `{"device_id":"…","account_uuid":"…","session_id":"<uuid>"}`
/// - 旧形式の文字列: `user_<hash>_account__session_<uuid>`
fn from_metadata_user_id(body: &Value) -> Option<String> {
    let raw = body.get("metadata")?.get("user_id")?.as_str()?;
    if let Some(rest) = raw.strip_prefix('{')
        && let Ok(obj) = serde_json::from_str::<Value>(&format!("{{{rest}"))
        && let Some(sid) = obj.get("session_id").and_then(Value::as_str)
        && !sid.is_empty()
    {
        return Some(sid.to_owned());
    }
    let tail = raw.rsplit_once("_session_")?.1;
    let id: String = tail
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    (!id.is_empty()).then_some(id)
}

/// 会話の冒頭 (system prompt + 最初の user 発話) からキーを作る。
///
/// assistant の応答は混ぜない。混ぜるとターンが進むたびにキーが変わり、
/// 同じ会話が別の credential に飛ぶ。cpa は「初回だけ短いキー、以降は長いキー」
/// の 2 段構えで衝突を避けているが、この gateway が扱う量では衝突より
/// キーの安定を優先する (DR-0002)。
fn message_hash(body: &Value) -> String {
    let mut h = DefaultHasher::new();
    if let Some(s) = text_of(body.get("system")) {
        "sys:".hash(&mut h);
        s.hash(&mut h);
    }
    if let Some(msgs) = body.get("messages").and_then(Value::as_array)
        && let Some(first_user) = msgs
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        && let Some(s) = text_of(first_user.get("content"))
    {
        "usr:".hash(&mut h);
        s.hash(&mut h);
    }
    format!("msg:{:016x}", h.finish())
}

/// Anthropic の content は文字列にもブロック配列にもなる。text だけ拾って繋ぐ。
fn text_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(blocks) => {
            let joined: String = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, v)| k.eq_ignore_ascii_case(name) && !v.trim().is_empty())
        .map(|(_, v)| v.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Claude Code 2.1.220 が実際に送る形 (2026-07-27 に実機で観測)。
    #[test]
    fn claude_code_json_user_id() {
        let body = json!({
            "metadata": {"user_id": r#"{"device_id":"d4bf…","account_uuid":"d6ca…","session_id":"8f17d3dd-a23f-4066-bf6c-3b8a8f0dc349"}"#},
        });
        assert_eq!(
            derive(&body, &[]).as_str(),
            "claude:8f17d3dd-a23f-4066-bf6c-3b8a8f0dc349"
        );
    }

    #[test]
    fn claude_code_legacy_user_id() {
        let body = json!({"metadata": {"user_id": "user_abc123_account__session_deadbeef-1234"}});
        assert_eq!(derive(&body, &[]).as_str(), "claude:deadbeef-1234");
    }

    /// ヘッダより metadata が優先される。両方あっても metadata を見る。
    #[test]
    fn metadata_wins_over_header() {
        let body = json!({"metadata": {"user_id": r#"{"session_id":"from-body"}"#}});
        let headers = vec![("X-Session-ID".into(), "from-header".into())];
        assert_eq!(derive(&body, &headers).as_str(), "claude:from-body");
    }

    #[test]
    fn header_fallbacks_in_priority_order() {
        let body = json!({});
        let cases = [
            (vec![("X-Session-ID", "a")], "header:a"),
            (vec![("Session-Id", "b")], "codex:b"),
            (vec![("session_id", "c")], "codex:c"),
            (vec![("X-Client-Request-Id", "d")], "clientreq:d"),
        ];
        for (hs, want) in cases {
            let hs: Vec<_> = hs
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            assert_eq!(derive(&body, &hs).as_str(), want);
        }
    }

    /// 名前の大小は問わない。値が空白だけのヘッダは無いものとして扱う。
    #[test]
    fn header_lookup_is_case_insensitive_and_skips_blank() {
        let body = json!({});
        let headers = vec![
            ("x-session-id".into(), "   ".into()),
            ("SESSION-ID".into(), "picked".into()),
        ];
        assert_eq!(derive(&body, &headers).as_str(), "codex:picked");
    }

    #[test]
    fn conversation_id_before_hash() {
        let body = json!({"conversation_id": "conv-1"});
        assert_eq!(derive(&body, &[]).as_str(), "conv:conv-1");
    }

    /// 識別子が何も無ければ会話の冒頭から作る。
    #[test]
    fn falls_back_to_message_hash() {
        let body = json!({
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "hello"}],
        });
        assert!(derive(&body, &[]).as_str().starts_with("msg:"));
    }

    /// ターンが進んでもキーは変わらない (= 会話が別の credential に飛ばない)。
    #[test]
    fn message_hash_is_stable_across_turns() {
        let first = json!({
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "hello"}],
        });
        let later = json!({
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "and then?"},
            ],
        });
        assert_eq!(derive(&first, &[]), derive(&later, &[]));
    }

    /// 別の会話は別のキーになる。
    #[test]
    fn different_conversations_differ() {
        let a = json!({"messages": [{"role": "user", "content": "hello"}]});
        let b = json!({"messages": [{"role": "user", "content": "goodbye"}]});
        assert_ne!(derive(&a, &[]), derive(&b, &[]));
    }

    /// content がブロック配列でも text を拾う (Claude Code はこの形で送る)。
    #[test]
    fn content_blocks_are_flattened() {
        let blocks = json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        });
        let plain = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert_eq!(derive(&blocks, &[]), derive(&plain, &[]));
    }

    /// 空のリクエストでもキーは返る (panic しない)。
    #[test]
    fn empty_body_yields_a_key() {
        assert!(derive(&json!({}), &[]).as_str().starts_with("msg:"));
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// 名乗りのヘッダは大文字小文字を問わず読む。相手の書き方に合わせない。
    #[test]
    fn the_declared_id_is_read_in_any_case() {
        for name in [
            "x-claude-code-session-id",
            "X-Claude-Code-Session-Id",
            "X-CLAUDE-CODE-SESSION-ID",
        ] {
            assert_eq!(
                declared_id(&headers(&[(name, "s-1")])),
                Some("s-1".to_owned()),
                "{name}"
            );
        }
    }

    /// 名乗らないクライアント (curl 等) もいる。会話を特定しないだけ。
    #[test]
    fn a_request_without_the_header_declares_nothing() {
        assert_eq!(declared_id(&[]), None);
        assert_eq!(declared_id(&headers(&[("content-type", "json")])), None);
        assert_eq!(declared_id(&headers(&[(SESSION_HEADER, "  ")])), None);
    }

    #[test]
    fn surrounding_space_is_trimmed_from_the_declared_id() {
        assert_eq!(
            declared_id(&headers(&[(SESSION_HEADER, " s-1 ")])),
            Some("s-1".to_owned())
        );
    }
}
