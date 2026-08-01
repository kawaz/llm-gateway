//! Anthropic Messages API を話す upstream。
//!
//! 転送とストリーム中継の実体はここが持ち、upstream ごとの差は
//! [`Provider`] の実装に閉じる。Bedrock は公式と同じ API を提供していて
//! (SSE の形式まで同一)、違うのは接続先・認証方式・モデル名・
//! 受け付ける beta フラグだけだった。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::credential::Credential;
use crate::{Error, Result};

pub mod beta;
pub mod forward;
pub mod provider;

pub use provider::{Bedrock, Official, Relay};

/// Messages API を話す upstream ごとの差分。
pub trait Provider: Send + Sync {
    /// ログや失敗記録に出す名前。
    fn name(&self) -> &str;

    /// 転送先の根 (`/v1/messages` などを足す前)。
    fn base_url(&self) -> &str;

    /// この upstream を使うのに認証情報が要るか。
    ///
    /// 転送先が自前で認証を持つ場合 (別 gateway へ渡すとき) は要らない。
    fn needs_credential(&self) -> bool {
        true
    }

    /// 認証ヘッダを載せる。方式は upstream ごとに違う。
    fn authorize(&self, headers: &mut Headers, credential: Option<&Credential>) -> Result<()>;

    /// この upstream に送るときの beta の既定。
    ///
    /// 適用は [`adapt`](Self::adapt) ではなく転送側が行う。credential が
    /// 学習した拒否リストを足したうえで、何を載せたかを控え、400 なら
    /// 落として送り直す — という一連が provider 1 つでは完結しないため
    /// (DR-0003)。
    fn beta_policy(&self) -> beta::Policy {
        beta::Policy::Passthrough
    }

    /// リクエストを upstream の要求に合わせる。
    ///
    /// 既定は素通し。公式 Anthropic はこれで足りる。
    fn adapt(&self, _body: &mut Value, _headers: &mut Headers) {}
}

/// ヘッダの入れ物。
///
/// 名前の大小を無視して引ける必要がある一方、upstream へは受け取ったままの
/// 表記で送りたい。`HeaderMap` を使わないのは、ここで扱うのが
/// 「クライアントから来たものを加工して渡す」だけで、型付けの利点が薄いため。
#[derive(Debug, Clone, Default)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn new(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 同じ名前があれば置き換え、無ければ足す。
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        match self
            .0
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            Some((_, v)) => *v = value.into(),
            None => self.0.push((name.to_owned(), value.into())),
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.0.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn extend_from(&mut self, extra: &BTreeMap<String, String>) {
        for (k, v) in extra {
            self.set(k, v.clone());
        }
    }

    /// クライアント由来のヘッダのうち、upstream へ渡さないもの。
    ///
    /// 残りは触らない — 何が意味を持つかは upstream が決めることで、
    /// gateway が判断すると新しいヘッダが増えるたびに落としてしまう。
    pub fn strip_for_upstream(&mut self) {
        const DROP: &[&str] = &[
            // 接続に紐づくもの (hop-by-hop)。
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "host",
            "content-length",
            // 認証は upstream ごとに付け直す。
            "authorization",
            "x-api-key",
            // 圧縮は転送側の都合で決める。
            "accept-encoding",
            // 手前のプロキシが付けた経路情報。upstream から見れば
            // この gateway がクライアントなので、こちら側の内部経路
            // (LAN や tailnet の IP、内向きのホスト名) を渡す理由がない。
            "x-forwarded-for",
            "x-forwarded-proto",
            "x-forwarded-host",
            "x-forwarded-port",
            "x-real-ip",
            "forwarded",
            "via",
        ];
        self.0
            .retain(|(k, _)| !DROP.iter().any(|d| k.eq_ignore_ascii_case(d)));
    }
}

/// ボディの `model` を upstream が求める名前に替える。
///
/// Bedrock は `anthropic.claude-fable-5` を要求し、クライアントが送る
/// `claude-fable-5` では 404 になる。
pub fn rewrite_model(body: &mut Value, upstream_name: &str) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_owned(), Value::String(upstream_name.to_owned()));
    }
}

/// ボディからモデル名を読む。
pub fn model_of(body: &Value) -> Result<&str> {
    body.get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Config("リクエストに model がありません".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        Headers::new(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn lookup_ignores_case() {
        let h = headers(&[("Anthropic-Version", "2023-06-01")]);
        assert_eq!(h.get("anthropic-version"), Some("2023-06-01"));
        assert_eq!(h.get("ANTHROPIC-VERSION"), Some("2023-06-01"));
        assert_eq!(h.get("missing"), None);
    }

    /// 上書きは表記が違っても同じヘッダとみなす (二重に載せない)。
    #[test]
    fn set_replaces_regardless_of_case() {
        let mut h = headers(&[("Authorization", "old")]);
        h.set("authorization", "new");

        let all: Vec<_> = h.iter().collect();
        assert_eq!(all.len(), 1, "重複させない: {all:?}");
        assert_eq!(h.get("authorization"), Some("new"));
    }

    #[test]
    fn set_appends_when_absent() {
        let mut h = Headers::default();
        h.set("x-api-key", "k");
        assert_eq!(h.get("X-Api-Key"), Some("k"));
    }

    #[test]
    fn remove_ignores_case() {
        let mut h = headers(&[("Anthropic-Beta", "a")]);
        h.remove("anthropic-beta");
        assert_eq!(h.get("anthropic-beta"), None);
    }

    /// 実クライアントが送るヘッダを通したとき、何が残り何が消えるか。
    #[test]
    fn strips_connection_and_auth_but_keeps_the_rest() {
        let mut h = headers(&[
            ("Host", "127.0.0.1:11300"),
            ("Connection", "keep-alive"),
            ("Content-Length", "253966"),
            ("Accept-Encoding", "gzip, deflate, br, zstd"),
            ("Authorization", "Bearer client-token"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "claude-code-20250219"),
            ("X-Claude-Code-Session-Id", "8f17d3dd"),
            ("User-Agent", "claude-cli/2.1.220"),
            ("x-app", "cli"),
        ]);
        h.strip_for_upstream();

        let names: Vec<&str> = h.iter().map(|(k, _)| k).collect();
        for gone in [
            "Host",
            "Connection",
            "Content-Length",
            "Accept-Encoding",
            "Authorization",
        ] {
            assert!(!names.contains(&gone), "{gone} は落とす: {names:?}");
        }
        for kept in [
            "anthropic-version",
            "anthropic-beta",
            "X-Claude-Code-Session-Id",
            "User-Agent",
            "x-app",
        ] {
            assert!(names.contains(&kept), "{kept} は残す: {names:?}");
        }
    }

    /// 認証は表記が違っても落とす (client が x-api-key を送ってきた場合)。
    #[test]
    fn strips_auth_regardless_of_case() {
        let mut h = headers(&[("X-API-KEY", "client-key"), ("AUTHORIZATION", "Bearer x")]);
        h.strip_for_upstream();
        assert_eq!(h.iter().count(), 0);
    }

    /// 手前のプロキシが付けた経路情報は upstream へ渡さない。
    ///
    /// 前段に Caddy を置くと、クライアントの LAN / tailnet の IP や
    /// 内向きのホスト名が乗ってくる。upstream から見えるのはこの gateway
    /// なので、こちら側の内部経路を知らせる理由がない。
    #[test]
    fn strips_proxy_added_route_information() {
        let mut h = headers(&[
            ("X-Forwarded-For", "100.76.5.89"),
            ("X-Forwarded-Proto", "https"),
            ("X-Forwarded-Host", "llm-gateway.example.internal"),
            ("X-Forwarded-Port", "443"),
            ("X-Real-IP", "100.76.5.89"),
            ("Forwarded", "for=100.76.5.89;proto=https"),
            ("Via", "1.1 caddy"),
            ("anthropic-version", "2023-06-01"),
        ]);
        h.strip_for_upstream();

        let names: Vec<&str> = h.iter().map(|(k, _)| k).collect();
        assert_eq!(
            names,
            vec!["anthropic-version"],
            "経路情報だけ落として残りは触らない: {names:?}"
        );
    }

    #[test]
    fn extra_headers_from_config_are_applied() {
        let mut h = headers(&[("anthropic-version", "2023-06-01")]);
        let extra = BTreeMap::from([("x-custom".to_owned(), "v".to_owned())]);
        h.extend_from(&extra);
        assert_eq!(h.get("x-custom"), Some("v"));
        assert_eq!(h.get("anthropic-version"), Some("2023-06-01"));
    }

    #[test]
    fn model_is_rewritten_in_place() {
        let mut body = json!({"model": "claude-fable-5", "max_tokens": 8});
        rewrite_model(&mut body, "anthropic.claude-fable-5");

        assert_eq!(body["model"], "anthropic.claude-fable-5");
        assert_eq!(body["max_tokens"], 8, "他の項目は触らない");
    }

    #[test]
    fn reads_model_name() {
        assert_eq!(
            model_of(&json!({"model": "claude-opus-5"})).unwrap(),
            "claude-opus-5"
        );
        assert!(model_of(&json!({})).is_err());
        assert!(model_of(&json!({"model": ""})).is_err());
        assert!(model_of(&json!({"model": 42})).is_err());
    }
}
