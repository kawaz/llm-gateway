//! Anthropic Messages 正規形から Responses API request への変換。

use serde_json::{Map, Value, json};

use crate::{Error, Result};

pub fn convert(body: Value) -> Result<Value> {
    let input = body
        .as_object()
        .ok_or_else(|| invalid("request body は JSON object で指定してください"))?;
    let model = input
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("model がありません"))?;

    let mut output = Map::new();
    output.insert("model".to_owned(), Value::String(model.to_owned()));
    output.insert("store".to_owned(), Value::Bool(false));
    output.insert("stream".to_owned(), Value::Bool(true));
    output.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));

    if let Some(system) = input.get("system") {
        output.insert(
            "instructions".to_owned(),
            Value::String(system_text(system)?),
        );
    }
    output.insert(
        "input".to_owned(),
        Value::Array(messages(input.get("messages"))?),
    );

    if let Some(tools) = input.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| invalid("tools は配列で指定してください"))?;
        output.insert(
            "tools".to_owned(),
            Value::Array(tools.iter().map(tool).collect::<Result<_>>()?),
        );
        output.insert("parallel_tool_calls".to_owned(), Value::Bool(false));
    }
    if let Some(choice) = input.get("tool_choice") {
        output.insert("tool_choice".to_owned(), tool_choice(choice)?);
    }
    if let Some(reasoning) = reasoning(
        input.get("thinking"),
        input.get("output_config").and_then(|c| c.get("effort")),
    )? {
        output.insert("reasoning".to_owned(), reasoning);
    }

    for key in ["temperature", "top_p"] {
        if let Some(value) = input.get(key) {
            output.insert(key.to_owned(), value.clone());
        }
    }
    for key in [
        "max_tokens",
        "stop_sequences",
        "top_k",
        "metadata",
        "service_tier",
    ] {
        if input.contains_key(key) {
            tracing::warn!(
                parameter = key,
                "Codex backend へ送れないパラメータを落とします"
            );
        }
    }

    Ok(Value::Object(output))
}

fn system_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                let kind = block.get("type").and_then(Value::as_str);
                if kind != Some("text") {
                    return Err(invalid("system には text block だけを指定できます"));
                }
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("system text block に text がありません"))
            })
            .collect::<Result<Vec<_>>>()
            .map(|parts| parts.join("\n\n")),
        _ => Err(invalid(
            "system は文字列または text block 配列で指定してください",
        )),
    }
}

fn messages(value: Option<&Value>) -> Result<Vec<Value>> {
    let messages = value
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("messages は配列で指定してください"))?;
    let mut output = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("message に role がありません"))?;
        let content = message
            .get("content")
            .ok_or_else(|| invalid("message に content がありません"))?;
        match role {
            "user" => user_content(content, &mut output)?,
            "assistant" => assistant_content(content, &mut output)?,
            _ => {
                return Err(invalid(
                    "message role は user または assistant にしてください",
                ));
            }
        }
    }
    Ok(output)
}

fn blocks(value: &Value) -> Result<Vec<Value>> {
    match value {
        Value::String(text) => Ok(vec![json!({"type": "text", "text": text})]),
        Value::Array(blocks) => Ok(blocks.clone()),
        _ => Err(invalid(
            "message content は文字列または配列で指定してください",
        )),
    }
}

fn user_content(value: &Value, output: &mut Vec<Value>) -> Result<()> {
    let mut parts = Vec::new();
    for block in blocks(value)? {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(json!({
                "type": "input_text",
                "text": required_str(&block, "text", "user text block")?,
            })),
            Some("image") => parts.push(json!({
                "type": "input_image",
                "image_url": image_url(&block)?,
            })),
            Some("tool_result") => {
                if !parts.is_empty() {
                    output.push(json!({"role": "user", "content": parts}));
                    parts = Vec::new();
                }
                output.push(json!({
                    "type": "function_call_output",
                    "call_id": required_str(&block, "tool_use_id", "tool_result")?,
                    "output": tool_result_text(block.get("content"))?,
                }));
            }
            Some(other) => {
                return Err(invalid(format!(
                    "user content block `{other}` は変換できません"
                )));
            }
            None => return Err(invalid("user content block に type がありません")),
        }
    }
    if !parts.is_empty() {
        output.push(json!({"role": "user", "content": parts}));
    }
    Ok(())
}

fn assistant_content(value: &Value, output: &mut Vec<Value>) -> Result<()> {
    let mut parts = Vec::new();
    for block in blocks(value)? {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(json!({
                "type": "output_text",
                "text": required_str(&block, "text", "assistant text block")?,
            })),
            Some("tool_use") => {
                if !parts.is_empty() {
                    output.push(json!({"role": "assistant", "content": parts}));
                    parts = Vec::new();
                }
                let id = required_str(&block, "id", "tool_use")?;
                output.push(json!({
                    "type": "function_call",
                    "id": id,
                    "call_id": id,
                    "name": required_str(&block, "name", "tool_use")?,
                    "arguments": serde_json::to_string(block.get("input").unwrap_or(&Value::Null))?,
                }));
            }
            Some("thinking" | "redacted_thinking") => {
                tracing::warn!("履歴中の thinking block を落とします");
            }
            Some(other) => {
                return Err(invalid(format!(
                    "assistant content block `{other}` は変換できません"
                )));
            }
            None => return Err(invalid("assistant content block に type がありません")),
        }
    }
    if !parts.is_empty() {
        output.push(json!({"role": "assistant", "content": parts}));
    }
    Ok(())
}

fn image_url(block: &Value) -> Result<String> {
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("image block に source がありません"))?;
    match source.get("type").and_then(Value::as_str) {
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid("image URL がありません")),
        Some("base64") => {
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("base64 image に media_type がありません"))?;
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("base64 image に data がありません"))?;
            Ok(format!("data:{media};base64,{data}"))
        }
        _ => Err(invalid("image source は url または base64 にしてください")),
    }
}

fn tool_result_text(value: Option<&Value>) -> Result<String> {
    match value {
        None => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(invalid("tool_result には text block だけを指定できます"));
                }
                required_str(block, "text", "tool_result text block").map(str::to_owned)
            })
            .collect::<Result<Vec<_>>>()
            .map(|parts| parts.join("\n")),
        Some(_) => Err(invalid(
            "tool_result content は文字列または text block 配列にしてください",
        )),
    }
}

fn tool(value: &Value) -> Result<Value> {
    let name = required_str(value, "name", "tool")?;
    let parameters = value
        .get("input_schema")
        .ok_or_else(|| invalid(format!("tool `{name}` は function tool ではありません")))?;
    Ok(json!({
        "type": "function",
        "name": name,
        "description": value.get("description").and_then(Value::as_str).unwrap_or(""),
        "parameters": parameters,
    }))
}

fn tool_choice(value: &Value) -> Result<Value> {
    match value.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(Value::String("auto".to_owned())),
        Some("any") => Ok(Value::String("required".to_owned())),
        Some("none") => Ok(Value::String("none".to_owned())),
        Some("tool") => Ok(json!({
            "type": "function",
            "name": required_str(value, "name", "tool_choice")?,
        })),
        _ => Err(invalid("tool_choice の type を変換できません")),
    }
}

/// 思考の深さを Responses API の `reasoning.effort` へ写す。
///
/// 深さの指定は 2 通りある。`output_config.effort` は段階を直接名指しするので
/// そのまま採り、`thinking` しか無ければ形から段階を導く。両方あれば名指しが勝つ。
/// `thinking: disabled` だけは「考えるな」という別の指示なので、段階の指定に
/// かかわらず reasoning を送らない。
fn reasoning(thinking: Option<&Value>, effort: Option<&Value>) -> Result<Option<Value>> {
    if thinking
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        == Some("disabled")
    {
        return Ok(None);
    }
    if let Some(effort) = effort {
        return Ok(Some(json!({"effort": named_effort(effort)?})));
    }
    let Some(thinking) = thinking else {
        return Ok(None);
    };
    let effort = match thinking.get("type").and_then(Value::as_str) {
        Some("adaptive") => "high",
        Some("enabled") => match thinking.get("budget_tokens").and_then(Value::as_u64) {
            Some(0..=2048) => "low",
            Some(16000..) => "high",
            _ => "medium",
        },
        _ => return Err(invalid("thinking の type を変換できません")),
    };
    Ok(Some(json!({"effort": effort})))
}

/// 名指しの段階を Responses API の語彙へ。
///
/// `max` は Responses API に無いので、いちばん深い `xhigh` に丸める。
fn named_effort(value: &Value) -> Result<&'static str> {
    match value.as_str() {
        Some("low") => Ok("low"),
        Some("medium") => Ok("medium"),
        Some("high") => Ok("high"),
        Some("xhigh") => Ok("xhigh"),
        Some("max") => Ok("xhigh"),
        _ => Err(invalid("output_config.effort を変換できません")),
    }
}

fn required_str<'a>(value: &'a Value, field: &str, context: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{context} に {field} がありません")))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Config(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code の通常 request に現れる text / tool / sampling を Responses へ写す。
    #[test]
    fn converts_text_tools_and_supported_parameters() {
        let converted = convert(json!({
            "model": "gpt-5.3-codex",
            "system": [{"type":"text","text":"one"},{"type":"text","text":"two"}],
            "messages": [
                {"role":"user","content":"hello"},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"hidden","signature":"sig"},
                    {"type":"tool_use","id":"call-1","name":"read","input":{"path":"a"}}
                ]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"ok"}]}
            ],
            "tools":[{"name":"read","description":"read it","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"auto"},
            "thinking":{"type":"enabled","budget_tokens":2048},
            "temperature":0.2,
            "top_p":0.9,
            "max_tokens":100
        })).unwrap();

        assert_eq!(converted["instructions"], "one\n\ntwo");
        assert_eq!(converted["store"], false);
        assert_eq!(converted["stream"], true);
        assert_eq!(converted["reasoning"]["effort"], "low");
        assert_eq!(converted["temperature"], 0.2);
        assert_eq!(converted["top_p"], 0.9);
        assert!(converted.get("max_tokens").is_none());
        assert!(
            converted["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call")
        );
        assert!(
            converted["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call_output")
        );
    }

    /// 構造を保てない block だけは、別の意味へ偽装せずに断る。
    #[test]
    fn rejects_structurally_untranslatable_blocks() {
        for body in [
            json!({"model":"m","system":[{"type":"image"}],"messages":[]}),
            json!({"model":"m","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":[{"type":"image"}]}]}]}),
            json!({"model":"m","messages":[],"tools":[{"name":"web_search"}]}),
        ] {
            assert!(convert(body).is_err());
        }
    }

    /// thinking budget の境界は low / medium / high の変換表どおり。
    #[test]
    fn maps_reasoning_effort_boundaries() {
        for (budget, expected) in [
            (2048, "low"),
            (2049, "medium"),
            (15999, "medium"),
            (16000, "high"),
        ] {
            assert_eq!(
                reasoning(
                    Some(&json!({"type":"enabled","budget_tokens":budget})),
                    None
                )
                .unwrap()
                .unwrap()["effort"],
                expected
            );
        }
    }

    /// 名指しの段階 (`output_config.effort`) は形からの推定より優先する。
    /// `max` は Responses API に無いので `xhigh` に丸める。
    #[test]
    fn a_named_effort_wins_over_the_thinking_shape() {
        for (named, expected) in [
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
            ("xhigh", "xhigh"),
            ("max", "xhigh"),
        ] {
            // thinking は budget から low になる形。名指しが勝つ。
            let converted = reasoning(
                Some(&json!({"type":"enabled","budget_tokens":1024})),
                Some(&json!(named)),
            )
            .unwrap()
            .unwrap();
            assert_eq!(converted["effort"], expected);
        }

        // 名指しだけでも効く。
        assert_eq!(
            reasoning(None, Some(&json!("xhigh"))).unwrap().unwrap()["effort"],
            "xhigh"
        );
        // 「考えるな」は段階の指定より強い。
        assert!(
            reasoning(Some(&json!({"type":"disabled"})), Some(&json!("high")))
                .unwrap()
                .is_none()
        );
        // 知らない段階は黙って落とさず弾く。
        assert!(reasoning(None, Some(&json!("turbo"))).is_err());
    }

    /// `output_config.effort` を送れば Responses API へ届く。
    #[test]
    fn an_output_config_effort_reaches_the_request() {
        let converted = convert(json!({
            "model": "gpt-5.6-luna",
            "max_tokens": 64,
            "output_config": {"effort": "xhigh"},
            "messages": [{"role":"user","content":"hi"}],
        }))
        .unwrap();
        assert_eq!(converted["reasoning"]["effort"], "xhigh");
    }
}
