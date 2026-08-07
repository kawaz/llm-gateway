//! Codex backend の quota・拒否・token usage を正規形へ写す。

use serde_json::Value;

use crate::denial::{DEFAULT_BACKOFF, Denial, RESET_SLACK, Reason, Scope};
use crate::egress::Headers;
use crate::metering::{Pricing, TokenKind, TokenUsage, UsageObserver};
use crate::provider::Metering;
use crate::quota::{Overage, Snapshot, Window};

const MAX_EVENT: usize = 256 * 1024;
const MAX_BACKOFF: i64 = 7 * 24 * 60 * 60;

pub struct OpenAiMetering;

impl Metering for OpenAiMetering {
    fn quota_snapshot(&self, headers: &Headers, observed_at: i64) -> Option<Snapshot> {
        let window = |name: &str| {
            let percent = header_f64(headers, &format!("x-codex-{name}-used-percent"));
            let reset = header_i64(headers, &format!("x-codex-{name}-reset-at"));
            let status = percent.map(|used| {
                if used >= 100.0 {
                    "rejected".to_owned()
                } else {
                    "allowed".to_owned()
                }
            });
            let window = Window {
                utilization: percent.map(|value| value / 100.0),
                status,
                ..Window::default()
            }
            .with_reset(reset);
            (!window.is_empty()).then_some(window)
        };
        let reached = headers
            .get("x-codex-rate-limit-reached-type")
            .map(str::to_owned);
        let overage = Overage {
            status: headers.get("x-codex-credits-has-credits").map(|value| {
                match value.eq_ignore_ascii_case("true") {
                    true => "available".to_owned(),
                    false => "unavailable".to_owned(),
                }
            }),
            disabled_reason: reached,
        };
        Snapshot::new(
            observed_at,
            window("primary"),
            window("secondary"),
            (!overage.is_empty()).then_some(overage),
        )
    }

    fn rejection(
        &self,
        status: u16,
        headers: &Headers,
        body: Option<&[u8]>,
        model: &str,
        observed_at: i64,
    ) -> Option<Denial> {
        if status != 429 {
            return None;
        }
        let reset = ["primary", "secondary"]
            .into_iter()
            .filter(|name| {
                header_f64(headers, &format!("x-codex-{name}-used-percent"))
                    .is_some_and(|used| used >= 100.0)
            })
            .filter_map(|name| header_i64(headers, &format!("x-codex-{name}-reset-at")))
            .filter(|reset| *reset > observed_at)
            .max()
            .or_else(|| reset_from_body(body, observed_at));
        if let Some(reset) = reset {
            return Some(Denial {
                until: reset + RESET_SLACK,
                reason: Reason::Limited,
                scope: Scope::Everything,
            });
        }
        let after = headers
            .get("retry-after")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(DEFAULT_BACKOFF)
            .clamp(0, MAX_BACKOFF);
        Some(Denial {
            until: observed_at + after,
            reason: Reason::Busy,
            scope: Scope::Model(model.to_owned()),
        })
    }

    fn usage_observer(&self, content_type: Option<&str>) -> Option<Box<dyn UsageObserver>> {
        let is_sse = content_type?
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
        is_sse.then(|| Box::new(OpenAiUsage::default()) as Box<dyn UsageObserver>)
    }

    fn pricing(&self, model: &str) -> Option<Pricing> {
        crate::preset::pricing::for_model(model)
    }
}

fn reset_from_body(body: Option<&[u8]>, now: i64) -> Option<i64> {
    let body: Value = serde_json::from_slice(body?).ok()?;
    body.pointer("/error/resets_at")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .filter(|reset| *reset > now)
        .or_else(|| {
            body.pointer("/error/resets_in_seconds")
                .and_then(|value| {
                    value
                        .as_i64()
                        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                })
                .map(|seconds| now + seconds.clamp(0, MAX_BACKOFF))
        })
}

fn header_f64(headers: &Headers, name: &str) -> Option<f64> {
    headers.get(name)?.trim().parse().ok()
}

fn header_i64(headers: &Headers, name: &str) -> Option<i64> {
    headers.get(name)?.trim().parse().ok()
}

#[derive(Default)]
struct OpenAiUsage {
    held: Vec<u8>,
    event: Vec<u8>,
    usage: TokenUsage,
    given_up: bool,
}

impl OpenAiUsage {
    fn line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            self.finish_event();
            return;
        }
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if !self.event.is_empty() {
            self.event.push(b'\n');
        }
        self.event.extend_from_slice(data);
    }

    fn finish_event(&mut self) {
        let event = std::mem::take(&mut self.event);
        let Ok(value) = serde_json::from_slice::<Value>(&event) else {
            return;
        };
        let Some(usage) = value.get("usage") else {
            return;
        };
        for (field, kind) in [
            ("input_tokens", TokenKind::INPUT_NAME),
            ("output_tokens", TokenKind::OUTPUT_NAME),
            ("cache_read_input_tokens", TokenKind::INPUT_CACHE_READ_NAME),
            ("reasoning_output_tokens", TokenKind::OUTPUT_REASONING_NAME),
        ] {
            if let Some(count) = usage.get(field).and_then(Value::as_u64) {
                self.usage.set(kind, count);
            }
        }
    }
}

impl UsageObserver for OpenAiUsage {
    fn observe(&mut self, chunk: &[u8]) {
        if self.given_up {
            return;
        }
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.held);
                self.line(&line);
            } else if self.held.len() + self.event.len() >= MAX_EVENT {
                self.given_up = true;
                self.held.clear();
                self.event.clear();
                self.usage = TokenUsage::default();
                tracing::warn!("OpenAI usage event が大きすぎるため集計をやめます");
                return;
            } else {
                self.held.push(byte);
            }
        }
    }

    fn finish(mut self: Box<Self>) -> Option<TokenUsage> {
        if self.given_up {
            return None;
        }
        let line = std::mem::take(&mut self.held);
        if !line.is_empty() {
            self.line(&line);
        }
        self.finish_event();
        (!self.usage.is_empty()).then_some(self.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn headers(values: &[(&str, &str)]) -> Headers {
        Headers::new(
            values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    /// primary / secondary の percent と reset を共通 snapshot へ写す。
    #[test]
    fn reads_codex_quota_headers() {
        let snapshot = OpenAiMetering
            .quota_snapshot(
                &headers(&[
                    ("x-codex-primary-used-percent", "71"),
                    ("x-codex-primary-reset-at", "1800001000"),
                    ("x-codex-secondary-used-percent", "100"),
                    ("x-codex-secondary-reset-at", "1800005000"),
                ]),
                NOW,
            )
            .unwrap();
        assert_eq!(snapshot.five_hour.unwrap().utilization, Some(0.71));
        assert_eq!(
            snapshot.seven_day.unwrap().status.as_deref(),
            Some("rejected")
        );
    }

    /// 429 body の reset は Retry-After が無くても route 全体の期限になる。
    #[test]
    fn reads_reset_from_a_rate_limit_body() {
        let denial = OpenAiMetering
            .rejection(
                429,
                &Headers::default(),
                Some(br#"{"error":{"type":"usage_limit_reached","resets_in_seconds":90}}"#),
                "gpt-5.3-codex",
                NOW,
            )
            .unwrap();
        assert_eq!(denial.until, NOW + 90 + RESET_SLACK);
        assert_eq!(denial.scope, Scope::Everything);
    }

    /// reasoning は output の内数として保持し、独立した観測区分にも残す。
    #[test]
    fn reads_all_translated_usage_kinds() {
        let mut observer = OpenAiMetering
            .usage_observer(Some("text/event-stream"))
            .unwrap();
        observer.observe(br#"data: {"type":"message_delta","usage":{"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":3,"reasoning_output_tokens":2}}

"#);
        let usage = observer.finish().unwrap();
        assert_eq!(usage.get(&TokenKind::input()), Some(10));
        assert_eq!(usage.get(&TokenKind::output()), Some(7));
        assert_eq!(usage.get(&TokenKind::input_cache_read()), Some(3));
        assert_eq!(usage.get(&TokenKind::output_reasoning()), Some(2));
    }
}
