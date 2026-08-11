//! Responses SSE から Anthropic Messages SSE への変換。

use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::StreamExt as _;
use serde_json::{Value, json};

use crate::egress::BodyStream;
use crate::{Error, Result};

pub(crate) const MAX_EVENT: usize = 256 * 1024;

pub fn translate(body: BodyStream) -> BodyStream {
    let state = State {
        body,
        translator: Translator::default(),
        pending: VecDeque::new(),
        finished: false,
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            loop {
                if let Some(chunk) = state.pending.pop_front() {
                    return Some((Ok(chunk), state));
                }
                if state.finished {
                    return None;
                }
                match state.body.next().await {
                    Some(Ok(chunk)) => match state.translator.push(&chunk) {
                        Ok(chunks) => state.pending.extend(chunks),
                        Err(error) => {
                            state.finished = true;
                            return Some((Err(error), state));
                        }
                    },
                    Some(Err(error)) => {
                        state.finished = true;
                        return Some((Err(error), state));
                    }
                    None => {
                        state.finished = true;
                        match state.translator.finish() {
                            Ok(chunks) => state.pending.extend(chunks),
                            Err(error) => return Some((Err(error), state)),
                        }
                    }
                }
            }
        },
    ))
}

struct State {
    body: BodyStream,
    translator: Translator,
    pending: VecDeque<Bytes>,
    finished: bool,
}

#[derive(Default)]
struct Translator {
    held: Vec<u8>,
    event: Vec<u8>,
    started: bool,
    block: Option<Block>,
    next_index: u64,
    model: String,
    saw_tool: bool,
    failed: bool,
}

#[derive(Clone, Copy)]
enum Block {
    Text(u64),
    Tool(u64),
}

impl Translator {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>> {
        let mut output = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.held);
                output.extend(self.line(&line)?);
            } else {
                if self.held.len() + self.event.len() >= MAX_EVENT {
                    return Err(Error::Config(
                        "Responses SSE event が大きすぎます".to_owned(),
                    ));
                }
                self.held.push(byte);
            }
        }
        Ok(output)
    }

    fn finish(&mut self) -> Result<Vec<Bytes>> {
        let mut output = Vec::new();
        let line = std::mem::take(&mut self.held);
        if !line.is_empty() {
            output.extend(self.line(&line)?);
        }
        output.extend(self.finish_event()?);
        Ok(output)
    }

    fn line(&mut self, line: &[u8]) -> Result<Vec<Bytes>> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return self.finish_event();
        }
        let Some(data) = line.strip_prefix(b"data:") else {
            return Ok(Vec::new());
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if !self.event.is_empty() {
            self.event.push(b'\n');
        }
        self.event.extend_from_slice(data);
        Ok(Vec::new())
    }

    fn finish_event(&mut self) -> Result<Vec<Bytes>> {
        let raw = std::mem::take(&mut self.event);
        if raw.is_empty() || raw == b"[DONE]" {
            return Ok(Vec::new());
        }
        let event: Value = serde_json::from_slice(&raw)?;
        self.convert(&event)
    }

    fn convert(&mut self, event: &Value) -> Result<Vec<Bytes>> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(model) = event
            .pointer("/response/model")
            .or_else(|| event.get("model"))
            .and_then(Value::as_str)
        {
            self.model = model.to_owned();
        }
        let mut output = Vec::new();
        match kind {
            "response.output_item.added" => {
                let item = &event["item"];
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => {
                        self.ensure_started(&mut output);
                        self.close_block(&mut output);
                        let index = self.take_index();
                        self.block = Some(Block::Tool(index));
                        self.saw_tool = true;
                        output.push(sse(
                            "content_block_start",
                            json!({
                                "type":"content_block_start",
                                "index":index,
                                "content_block":{
                                    "type":"tool_use",
                                    "id":item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or("call"),
                                    "name":item.get("name").and_then(Value::as_str).unwrap_or("tool"),
                                    "input":{}
                                }
                            }),
                        ));
                    }
                    Some("message") => self.ensure_started(&mut output),
                    Some("reasoning") => {}
                    _ => {}
                }
            }
            "response.output_text.delta" | "response.content_part.delta" => {
                self.ensure_started(&mut output);
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .or_else(|| event.pointer("/part/text").and_then(Value::as_str))
                    .unwrap_or("");
                let index = match self.block {
                    Some(Block::Text(index)) => index,
                    _ => {
                        self.close_block(&mut output);
                        let index = self.take_index();
                        self.block = Some(Block::Text(index));
                        output.push(sse(
                            "content_block_start",
                            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
                        ));
                        index
                    }
                };
                output.push(sse(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":delta}}),
                ));
            }
            "response.function_call_arguments.delta" => {
                self.ensure_started(&mut output);
                let index = match self.block {
                    Some(Block::Tool(index)) => index,
                    _ => {
                        return Err(Error::Config(
                            "tool 引数が tool item より先に届きました".to_owned(),
                        ));
                    }
                };
                output.push(sse(
                    "content_block_delta",
                    json!({
                        "type":"content_block_delta",
                        "index":index,
                        "delta":{"type":"input_json_delta","partial_json":event.get("delta").and_then(Value::as_str).unwrap_or("")}
                    }),
                ));
            }
            "response.output_item.done" => self.close_block(&mut output),
            "response.completed" => {
                self.ensure_started(&mut output);
                self.close_block(&mut output);
                let response = &event["response"];
                let status = response.get("status").and_then(Value::as_str);
                let incomplete = response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str);
                let stop_reason = if status == Some("incomplete")
                    || matches!(incomplete, Some("max_tokens" | "max_output_tokens"))
                {
                    "max_tokens"
                } else if self.saw_tool {
                    "tool_use"
                } else {
                    "end_turn"
                };
                output.push(sse(
                    "message_delta",
                    json!({
                        "type":"message_delta",
                        "delta":{"stop_reason":stop_reason,"stop_sequence":null},
                        "usage":usage(response.get("usage"))
                    }),
                ));
                output.push(sse("message_stop", json!({"type":"message_stop"})));
            }
            "error" | "response.failed" if !self.failed => {
                self.failed = true;
                let error = event
                    .get("error")
                    .or_else(|| event.pointer("/response/error"))
                    .unwrap_or(event);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("OpenAI response failed");
                output.push(sse(
                    "error",
                    json!({
                        "type":"error",
                        "error":{
                            "type":error_type(message),
                            "message":message
                        }
                    }),
                ));
            }
            _ => {}
        }
        Ok(output)
    }

    fn ensure_started(&mut self, output: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        output.push(sse(
            "message_start",
            json!({
                "type":"message_start",
                "message":{
                    "id":"msg_openai",
                    "type":"message",
                    "role":"assistant",
                    "model":self.model,
                    "content":[],
                    "stop_reason":null,
                    "stop_sequence":null,
                    "usage":{"input_tokens":0,"output_tokens":0}
                }
            }),
        ));
    }

    fn close_block(&mut self, output: &mut Vec<Bytes>) {
        if let Some(block) = self.block.take() {
            let index = match block {
                Block::Text(index) | Block::Tool(index) => index,
            };
            output.push(sse(
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            ));
        }
    }

    fn take_index(&mut self) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }
}

fn error_type(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if message.contains("overload") || message.contains("try again later") {
        "overloaded_error"
    } else if message.contains("invalid request") || message.contains("invalid_request") {
        "invalid_request_error"
    } else {
        "api_error"
    }
}

fn usage(value: Option<&Value>) -> Value {
    let value = value.unwrap_or(&Value::Null);
    json!({
        "input_tokens": value.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        "output_tokens": value.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        "cache_read_input_tokens": value.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0),
        "reasoning_output_tokens": value.pointer("/output_tokens_details/reasoning_tokens").and_then(Value::as_u64).unwrap_or(0)
    })
}

fn sse(event: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    async fn translated(chunks: Vec<&'static [u8]>) -> String {
        let body: BodyStream = Box::pin(stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok(Bytes::from_static(chunk))),
        ));
        let bytes = crate::egress::collect_body(translate(body)).await.unwrap();
        String::from_utf8(bytes).unwrap()
    }

    /// text と tool call を Anthropic の block lifecycle に変換する。
    #[tokio::test]
    async fn converts_text_tool_and_usage_events() {
        let output = translated(vec![
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            b"data: {\"type\":\"response.output_item.done\"}\n\n",
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"path\\\":\\\"a\\\"}\"}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens_details\":{\"reasoning_tokens\":2}}}}\n\n",
        ]).await;

        assert!(output.contains("message_start"), "{output}");
        assert!(output.contains("text_delta"), "{output}");
        assert!(output.contains("tool_use"), "{output}");
        assert!(output.contains("input_json_delta"), "{output}");
        assert!(output.contains("\"stop_reason\":\"tool_use\""), "{output}");
        assert!(output.contains("\"cache_read_input_tokens\":3"), "{output}");
        assert!(output.contains("\"reasoning_output_tokens\":2"), "{output}");
    }

    /// 同じ upstream failure を表す error と response.failed は、クライアントへ 1 件だけ返す。
    #[tokio::test]
    async fn emits_one_error_for_a_single_failure() {
        let output = translated(vec![
            b"data: {\"type\":\"error\",\"error\":{\"message\":\"Our servers are currently overloaded. Please try again later.\"}}\n\n",
            b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"OpenAI response failed\"}}}\n\n",
        ])
        .await;

        assert_eq!(output.matches("event: error").count(), 1, "{output}");
        assert!(output.contains("\"type\":\"overloaded_error\""), "{output}");
    }

    /// chunk が SSE 行の途中で切れてもイベントを失わない。
    #[tokio::test]
    async fn survives_chunk_boundaries() {
        let output = translated(vec![
            b"data: {\"type\":\"response.output_text.",
            b"delta\",\"delta\":\"ok\"}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        ])
        .await;
        assert!(output.contains("\"text\":\"ok\""), "{output}");
        assert!(output.contains("message_stop"), "{output}");
    }
}
