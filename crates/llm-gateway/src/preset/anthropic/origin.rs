//! 呼び出し元 (メイン / サブエージェント / 1 回きり) の見分け方 (DR-0024)。
//!
//! Claude Code は `metadata.user_id` に JSON 文字列を載せてくる。その中の
//! `parent_session_id` が、そのリクエストを出したのが親から生えた
//! サブエージェントかどうかを表す (実測: docs/knowledge/2026-09-02-…)。
//!
//! 親を持たない呼び出しは、さらに `system` の先頭ブロックに載る請求ヘッダで
//! 分かれる。`cc_entrypoint=cli` が対話セッション、それ以外 (`sdk-cli` =
//! `claude -p` 等) は 1 回きりの呼び出し。
//!
//! ここが返すのは 3 値だけで、その値を何に使うかは core が決める
//! (DR-0014 の境界)。

use serde_json::Value;

use crate::provider::{CallerOrigin, RequestOrigin};

/// `metadata.user_id` から呼び出し元を読む。
pub struct MetadataOrigin;

impl CallerOrigin for MetadataOrigin {
    fn origin(&self, body: &Value) -> RequestOrigin {
        let Some(raw) = body
            .get("metadata")
            .and_then(|metadata| metadata.get("user_id"))
            .and_then(Value::as_str)
        else {
            return RequestOrigin::Unknown;
        };
        // 旧形式 (`user_<hash>_account__session_<uuid>`) には親の欄が無い。
        // 名乗り方が違うだけで「サブエージェントではない」とは言えないので、
        // 読めない形は Unknown のまま返す。
        let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(raw) else {
            return RequestOrigin::Unknown;
        };
        if fields
            .get("parent_session_id")
            .is_some_and(|parent| !parent.is_null())
        {
            return RequestOrigin::Sub;
        }
        match entrypoint(body) {
            // 対話セッション。続きが来る前提で扱ってよい。
            Some(INTERACTIVE) | None => RequestOrigin::Main,
            // `claude -p` のような 1 回きりの呼び出し。
            Some(_) => RequestOrigin::Oneshot,
        }
    }
}

/// 対話セッションが名乗る入り口。
const INTERACTIVE: &str = "cli";

/// `system` の先頭ブロックの請求ヘッダが名乗る入り口。
///
/// 形は `x-anthropic-billing-header: cc_version=<ver>; cc_entrypoint=<name>;`。
/// 先頭ブロックだけを見るのは、そこがクライアントの固定行だから
/// ([`crate::events::prefix`] と同じ理由)。
fn entrypoint(body: &Value) -> Option<&str> {
    const HEADER: &str = "x-anthropic-billing-header:";
    const FIELD: &str = "cc_entrypoint=";

    let head = body.pointer("/system/0/text")?.as_str()?;
    if !head.trim_start().starts_with(HEADER) {
        return None;
    }
    let value = head.split(FIELD).nth(1)?;
    let name = value
        .split(|c: char| c == ';' || c.is_whitespace())
        .next()?
        .trim();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn origin(user_id: Value) -> RequestOrigin {
        MetadataOrigin.origin(&json!({"metadata": {"user_id": user_id}}))
    }

    /// 親を持たない名乗りと、請求ヘッダに載る入り口の組み合わせ。
    fn from_entrypoint(entrypoint: &str) -> RequestOrigin {
        MetadataOrigin.origin(&json!({
            "metadata": {"user_id": r#"{"session_id":"8f17d3dd"}"#},
            "system": [{
                "type": "text",
                "text": format!("x-anthropic-billing-header: cc_version=2.0.1; cc_entrypoint={entrypoint};"),
            }],
        }))
    }

    /// 親を持たない名乗りはメイン系 (メイン本体・fork・直下の classifier)。
    #[test]
    fn a_request_without_a_parent_is_a_main_request() {
        assert_eq!(
            origin(json!(
                r#"{"device_id":"d4bf","account_uuid":"d6ca","session_id":"8f17d3dd"}"#
            )),
            RequestOrigin::Main
        );
    }

    /// 親の session を名乗るものはサブエージェント系。
    #[test]
    fn a_request_that_names_its_parent_is_a_subagent() {
        assert_eq!(
            origin(json!(
                r#"{"session_id":"8f17d3dd","parent_session_id":"1c0a4b22"}"#
            )),
            RequestOrigin::Sub
        );
    }

    /// 対話セッション以外の入り口 (`claude -p` 等) は 1 回きりの呼び出し。
    ///
    /// 会話として続かないので、続きを当て込んだ扱いをしても報われない。
    #[test]
    fn a_request_from_another_entrypoint_is_a_one_shot_call() {
        assert_eq!(from_entrypoint("sdk-cli"), RequestOrigin::Oneshot);
        assert_eq!(from_entrypoint("cli"), RequestOrigin::Main);
    }

    /// 請求ヘッダを持たない相手は、入り口で振り分けない。
    #[test]
    fn a_request_without_the_billing_header_is_a_main_request() {
        for system in [
            json!(null),
            json!([]),
            json!("むかしの書き方 (文字列)"),
            json!([{"type": "text", "text": "You are Claude Code"}]),
            json!([{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.0.1;"}]),
            // 入り口の名前は先頭ブロックでだけ見る。
            json!([
                {"type": "text", "text": "You are Claude Code"},
                {"type": "text", "text": "x-anthropic-billing-header: cc_entrypoint=sdk-cli;"},
            ]),
        ] {
            assert_eq!(
                MetadataOrigin.origin(&json!({
                    "metadata": {"user_id": r#"{"session_id":"8f17d3dd"}"#},
                    "system": system,
                })),
                RequestOrigin::Main,
                "{system}"
            );
        }
    }

    /// 親を名乗るなら、入り口が何であれサブエージェント。
    #[test]
    fn a_subagent_stays_a_subagent_whatever_its_entrypoint() {
        for entrypoint in ["cli", "sdk-cli"] {
            assert_eq!(
                MetadataOrigin.origin(&json!({
                    "metadata": {"user_id": r#"{"session_id":"a","parent_session_id":"b"}"#},
                    "system": [{
                        "type": "text",
                        "text": format!("x-anthropic-billing-header: cc_entrypoint={entrypoint};"),
                    }],
                })),
                RequestOrigin::Sub,
                "{entrypoint}"
            );
        }
    }

    /// 名乗りが無い / 読めない形では見分けが付かない。
    #[test]
    fn an_unreadable_request_has_no_known_origin() {
        for body in [
            json!({}),
            json!({"metadata": {}}),
            json!({"metadata": {"user_id": 42}}),
            json!({"metadata": {"user_id": "user_abc_account__session_deadbeef"}}),
            json!({"metadata": {"user_id": ""}}),
        ] {
            assert_eq!(
                MetadataOrigin.origin(&body),
                RequestOrigin::Unknown,
                "{body}"
            );
        }
    }
}
