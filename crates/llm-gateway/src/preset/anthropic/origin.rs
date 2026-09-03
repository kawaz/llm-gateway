//! 呼び出し元 (メイン / サブエージェント) の見分け方 (DR-0024)。
//!
//! Claude Code は `metadata.user_id` に JSON 文字列を載せてくる。その中の
//! `parent_session_id` が、そのリクエストを出したのが親から生えた
//! サブエージェントかどうかを表す (実測: docs/knowledge/2026-09-02-…)。
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
        match fields.get("parent_session_id") {
            Some(parent) if !parent.is_null() => RequestOrigin::Sub,
            _ => RequestOrigin::Main,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn origin(user_id: Value) -> RequestOrigin {
        MetadataOrigin.origin(&json!({"metadata": {"user_id": user_id}}))
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
