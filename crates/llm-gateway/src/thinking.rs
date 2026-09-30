//! account を跨いだ session の thinking を、assistant の text にして送る (DR-0033 §3)。
//!
//! 署名がもう効かないと分かっている session にだけ当てる。跨いだ後の署名は
//! account 束縛でどのみち捨てられるので、本文を変えても prefix 束縛で失うものは
//! 無い (DR-0024 の「本文を変えない」との線引きは DR-0033 の影響節)。
//!
//! 判断は純粋関数で、状態を持たない。同じ本文からは同じ本文を作る — 変わると
//! 送るたびに prompt cache が壊れる。

use serde_json::{Map, Value};

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
    fn a_body_without_thinking_is_left_as_is() {
        let body = json!({"model": "m", "input": "hi", "messages": [
            {"role": "user", "content": "plain"},
            {"role": "assistant", "content": [{"type": "text", "text": "x"}]},
        ]});
        assert_eq!(converted(body.clone()), body);
    }
}
