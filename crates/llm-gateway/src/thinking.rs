//! session の thinking の扱い (DR-0035 §3・§5)。
//!
//! - [`carries_thinking`] その 1 本の履歴が thinking を運ぶか。跨ぎの記録・判定の対象を決める
//! - [`as_text`] 跨いだ session の thinking を assistant の text にして送る
//!
//! text 化は署名がもう効かないと分かっている session にだけ当てる。跨いだ後の
//! 署名は model / account の束縛でどのみち捨てられるので、本文を変えても prefix
//! 束縛で失うものは無い (DR-0024 の「本文を変えない」との線引きは DR-0035 §5)。
//!
//! 判断は純粋関数で、状態を持たない。同じ本文からは同じ本文を作る — 変わると
//! 送るたびに prompt cache が壊れる。

use serde_json::{Map, Value};

/// この 1 本が thinking を運ぶか (DR-0035 §3)。assistant の content に
/// `thinking` / `redacted_thinking` block がある時だけ。
///
/// `thinking` param は見ない: thinking を切った model へ移っても、履歴の thinking
/// は跨ぎとして扱う (text 化すれば文脈に載る)。同じ session id で別 model が走る
/// 脇の呼び出し (権限判定・要約) は履歴に thinking を持たないので、本流の出所に
/// 混ざらない。見るのは正規形 (Messages 形式) だけで、他の形の本文は運ばない扱い。
pub fn carries_thinking(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .any(|block| {
            matches!(
                block.get("type").and_then(Value::as_str),
                Some("thinking" | "redacted_thinking")
            )
        })
}

/// 本文の thinking を text に置き換える。
///
/// - `thinking` → `{"type":"text","text":<本文>}`。本文は元の本文から末尾の
///   改行と半角空白だけを落としたもの (内部の改行はそのまま)。見出しや接頭辞は付けない (付けると
///   `reasoning_extraction` の refusal になる実測がある)。署名は捨てる
/// - `redacted_thinking` と、本文が空の `thinking` → 落とす (運ぶ本文が無い)
/// - 位置は元の block のまま。元の block の `cache_control` は置き換えた text へ移す
/// - 他の block には触らない
///
/// 見るのは正規形 (Messages 形式) の `messages[].content[]` だけ。他の形の本文
/// には thinking block の置き場が無いので、何も変わらない。
pub fn as_text(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let blocks = std::mem::take(content);
        content.extend(blocks.into_iter().filter_map(replace));
    }
}

/// 1 block を置き換える。落とすなら `None`。
fn replace(block: Value) -> Option<Value> {
    match block.get("type").and_then(Value::as_str) {
        Some("redacted_thinking") => None,
        Some("thinking") => {
            // 落とすのは末尾の改行と半角空白だけ。内部の改行 (段落の区切り) と
            // 他の空白 (タブ等) は本文として残す。
            let body = block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim_end_matches(['\n', ' ']);
            if body.is_empty() {
                return None;
            }
            let mut text = Map::new();
            text.insert("type".to_owned(), Value::from("text"));
            text.insert("text".to_owned(), Value::from(body));
            if let Some(cache_control) = block.get("cache_control") {
                text.insert("cache_control".to_owned(), cache_control.clone());
            }
            Some(Value::Object(text))
        }
        _ => Some(block),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn converted(body: Value) -> Value {
        let mut body = body;
        as_text(&mut body);
        body
    }

    #[test]
    fn thinking_becomes_text_keeping_its_newlines_with_the_end_trimmed() {
        let got = converted(json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "first\nsecond\n\nthird\n \n  ", "signature": "sig"},
                {"type": "text", "text": "answer\nkept"},
            ]},
        ]}));
        assert_eq!(
            got["messages"][1]["content"],
            json!([
                {"type": "text", "text": "first\nsecond\n\nthird"},
                {"type": "text", "text": "answer\nkept"},
            ])
        );
        assert_eq!(got["messages"][0], json!({"role": "user", "content": "hi"}));
    }

    #[test]
    fn only_trailing_newlines_and_ascii_spaces_are_trimmed() {
        let got = converted(json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "a\t\n \n", "signature": "s"},
                {"type": "thinking", "thinking": "b\u{3000}\n", "signature": "s"},
            ]},
        ]}));
        assert_eq!(
            got["messages"][0]["content"],
            json!([
                {"type": "text", "text": "a\t"},
                {"type": "text", "text": "b\u{3000}"},
            ])
        );
    }

    #[test]
    fn redacted_and_empty_thinking_are_dropped() {
        let got = converted(json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "thinking", "thinking": "", "signature": "sig"},
                {"type": "thinking", "thinking": "\n\n", "signature": "sig"},
                {"type": "tool_use", "id": "t1", "name": "x", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
            ]},
        ]}));
        assert_eq!(
            got["messages"][0]["content"],
            json!([{"type": "tool_use", "id": "t1", "name": "x", "input": {}}])
        );
        assert_eq!(
            got["messages"][1]["content"],
            json!([{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}])
        );
    }

    #[test]
    fn the_cache_control_of_a_thinking_block_moves_to_its_text() {
        let got = converted(json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "why", "signature": "s",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ]},
        ]}));
        assert_eq!(
            got["messages"][0]["content"],
            json!([{"type": "text", "text": "why", "cache_control": {"type": "ephemeral", "ttl": "1h"}}])
        );
    }

    #[test]
    fn the_same_body_converts_to_the_same_bytes() {
        let body = json!({"model": "m", "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "a\nb", "signature": "s"},
                {"type": "text", "text": "c"},
            ]},
        ]});
        let once = serde_json::to_vec(&converted(body.clone())).unwrap();
        let twice = serde_json::to_vec(&converted(body)).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn the_thinking_param_alone_does_not_carry() {
        for thinking in [
            json!({"type": "adaptive"}),
            json!({"type": "enabled", "budget_tokens": 1024}),
            json!({"type": "disabled"}),
        ] {
            let body = json!({"model": "m", "thinking": thinking, "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "text", "text": "x"}]},
                {"role": "user", "content": "next"},
            ]});
            assert!(!carries_thinking(&body), "{body}");
        }
    }

    #[test]
    fn a_body_without_history_does_not_carry() {
        assert!(!carries_thinking(
            &json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})
        ));
        assert!(!carries_thinking(&json!({"model": "m", "messages": []})));
    }

    #[test]
    fn thinking_blocks_in_the_history_carry_with_or_without_the_param() {
        for block in [
            json!({"type": "thinking", "thinking": "", "signature": "s"}),
            json!({"type": "redacted_thinking", "data": "opaque"}),
        ] {
            for param in [
                None,
                Some(json!({"type": "disabled"})),
                Some(json!({"type": "adaptive"})),
            ] {
                let mut body = json!({"model": "m", "messages": [
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": [block.clone(), {"type": "text", "text": "x"}]},
                ]});
                if let Some(param) = param {
                    body["thinking"] = param;
                }
                assert!(carries_thinking(&body), "{body}");
            }
        }
    }

    #[test]
    fn only_assistant_blocks_of_the_messages_shape_count() {
        let user_side = json!({"model": "m", "messages": [
            {"role": "user", "content": [{"type": "thinking", "thinking": "x", "signature": "s"}]},
            {"role": "assistant", "content": "plain"},
        ]});
        assert!(!carries_thinking(&user_side));
        let other_shape = json!({"model": "m", "input": [
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "x"}]},
        ]});
        assert!(!carries_thinking(&other_shape));
    }

    #[test]
    fn a_body_without_thinking_is_left_as_is() {
        let body = json!({"model": "m", "input": "hi", "messages": [
            {"role": "user", "content": "plain"},
            {"role": "assistant", "content": [{"type": "text", "text": "x"}]},
        ]});
        assert_eq!(converted(body.clone()), body);
    }
}
