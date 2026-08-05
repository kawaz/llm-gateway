//! Anthropic の応答を metering の正規形へ写す。
//!
//! 枠ヘッダの読み方、断られ方の意味、本文 usage のトークン区分、単価の課金軸は
//! どれもこの方言の知識なので preset 側が持つ (DR-0014 §4)。core が規定するのは
//! 写した先の形 ([`crate::metering`]) だけ。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::denial::{self, Denial};
use crate::egress::Headers;
use crate::metering::{Pricing, TokenKind, TokenUsage, UsageObserver};
use crate::provider::Metering;
use crate::quota::Snapshot;

/// SSE の 1 イベントをどこまで抱えるか。
///
/// 行の途中でチャンクが切れるので行が揃うまで持ち、さらに 1 イベントが複数の
/// `data:` 行に割れうるのでイベントが閉じるまで持つ。壊れた相手 (改行も空行も
/// 返さない upstream) にメモリを食い潰されないための上限で、実際の
/// `message_start` は 1KB 前後なので桁が違う。
const MAX_SSE_EVENT: usize = 256 * 1024;

/// ストリームでない応答を、集計のためにどこまで抱えるか。
const MAX_JSON_BODY: usize = 4 * 1024 * 1024;

/// Messages API の応答から読み取る。
pub struct AnthropicMetering;

impl Metering for AnthropicMetering {
    fn quota_snapshot(&self, headers: &Headers, observed_at: i64) -> Option<Snapshot> {
        Snapshot::from_headers(headers.as_slice(), observed_at)
    }

    fn rejection(
        &self,
        status: u16,
        headers: &Headers,
        model: &str,
        observed_at: i64,
    ) -> Option<Denial> {
        denial::denial_of(status, headers.as_slice(), model, observed_at)
    }

    fn usage_observer(&self, content_type: Option<&str>) -> Option<Box<dyn UsageObserver>> {
        Mode::of(content_type)
            .map(|mode| Box::new(MessagesUsage::new(mode)) as Box<dyn UsageObserver>)
    }

    /// 重複しない課金軸を選ぶ。
    ///
    /// Anthropic の `input_tokens` はキャッシュ分を含まないので、4 区分を
    /// そのまま並べても二重計上にならない。
    fn pricing(&self, model: &str) -> Option<Pricing> {
        let rates = crate::pricing::rates_for(model)?;
        Some(Pricing {
            rates: BTreeMap::from([
                (TokenKind::input(), rates.input),
                (TokenKind::output(), rates.output),
                (TokenKind::input_cache_creation(), rates.cache_write),
                (TokenKind::input_cache_read(), rates.cache_read),
            ]),
        })
    }
}

/// 本文のどの読み方をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// SSE。`data:` の行に usage が現れる。
    Sse,
    /// ひとまとまりの JSON。終端で `/usage` を読む。
    Json,
}

impl Mode {
    /// この content-type から usage を読めるか。
    ///
    /// 分からない形は読まない。中身を推測して読みに行くと、画像やバイナリを
    /// JSON として抱え込むことになる。
    fn of(content_type: Option<&str>) -> Option<Self> {
        // `application/json; charset=utf-8` のような付属物を落とす。
        let base = content_type?.split(';').next()?.trim().to_ascii_lowercase();
        match base.as_str() {
            "text/event-stream" => Some(Self::Sse),
            "application/json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// 通り過ぎたバイト列から usage を読む。本文は変えない。
struct MessagesUsage {
    mode: Mode,
    /// 行の途中 (SSE) / 本文の全部 (JSON) を溜める控え。
    held: Vec<u8>,
    /// 今のイベントで溜めた `data:` の中身 (SSE)。複数行なら改行で繋いである。
    event: Vec<u8>,
    /// 上限を超えたので、この応答の集計をやめた。
    given_up: bool,
    usage: TokenUsage,
}

impl MessagesUsage {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            held: Vec::new(),
            event: Vec::new(),
            given_up: false,
            usage: TokenUsage::default(),
        }
    }

    /// SSE を行に切り、イベントが閉じるところで中身を読む。
    ///
    /// チャンクの境目は行の途中に落ちる。揃った行だけを処理し、残りは次の
    /// チャンクまで持つ。
    fn observe_sse(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == b'\n' {
                let line = std::mem::take(&mut self.held);
                self.read_sse_line(&line);
                continue;
            }
            // 書きかけの行と、このイベントで溜めた分の合計で見る。
            if self.held.len() + self.event.len() >= MAX_SSE_EVENT {
                self.give_up("SSE の 1 イベントが長すぎます");
                return;
            }
            self.held.push(b);
        }
    }

    /// SSE の 1 行を処理する。
    ///
    /// 空行はイベントの終わり。`data:` の行は中身を溜めるだけで、読むのは
    /// イベントが閉じたとき — **1 つのイベントの data は複数行に割れてよく、
    /// その場合は改行で繋いだものが 1 つの中身**になる (SSE の仕様)。行ごとに
    /// 解こうとすると、そうやって割られた usage を黙って取りこぼす。
    fn read_sse_line(&mut self, line: &[u8]) {
        // 行末の `\r` は終端の一部 (CRLF で区切る upstream がある)。
        let line = line.strip_suffix(b"\r").unwrap_or(line);

        if line.is_empty() {
            self.finish_event();
            return;
        }
        let Some(payload) = line.strip_prefix(b"data:") else {
            // `event:` / `id:` / 注釈行は読まない。usage を載せるのは data だけ。
            return;
        };
        // コロンの直後の空白 1 つは区切りの一部で、中身には入らない。
        let payload = payload.strip_prefix(b" ").unwrap_or(payload);

        if !self.event.is_empty() {
            self.event.push(b'\n');
        }
        self.event.extend_from_slice(payload);
    }

    /// イベントが閉じた。溜めた中身から usage を読む。
    ///
    /// JSON として解くのは usage が載っているものだけ。イベントは 1 応答で
    /// 何十個も流れるので、全部解くと中継の脇で無駄に働くことになる。
    fn finish_event(&mut self) {
        let event = std::mem::take(&mut self.event);
        if !contains(&event, b"\"usage\"") {
            return;
        }
        let Ok(parsed) = serde_json::from_slice::<Value>(&event) else {
            return;
        };
        // `message_start` は `/message/usage`、`message_delta` は `/usage` に
        // 載せる。イベント名で決め打ちせず、在る方を読む。
        for pointer in ["/message/usage", "/usage"] {
            if let Some(usage) = parsed.pointer(pointer) {
                self.absorb(usage);
            }
        }
    }

    /// usage オブジェクトに載っている分を正規形へ写す。
    ///
    /// 値は累積で届くので**上書き**する (足さない)。載っていない区分は前に
    /// 拾った値を保つ — `message_delta` が一部しか載せない場合に備える。
    fn absorb(&mut self, usage: &Value) {
        const AXES: &[(&str, &str)] = &[
            ("input_tokens", TokenKind::INPUT_NAME),
            ("output_tokens", TokenKind::OUTPUT_NAME),
            (
                "cache_creation_input_tokens",
                TokenKind::INPUT_CACHE_CREATION_NAME,
            ),
            ("cache_read_input_tokens", TokenKind::INPUT_CACHE_READ_NAME),
        ];
        for (field, kind) in AXES {
            if let Some(count) = usage.get(field).and_then(Value::as_u64) {
                self.usage.set(*kind, count);
            }
        }
    }

    /// 上限を超えた。この応答の集計は捨てる。
    fn give_up(&mut self, reason: &str) {
        self.given_up = true;
        self.held = Vec::new();
        self.event = Vec::new();
        self.usage = TokenUsage::default();
        tracing::warn!(reason, "使用量の集計をやめます");
    }
}

impl UsageObserver for MessagesUsage {
    fn observe(&mut self, chunk: &[u8]) {
        if self.given_up {
            return;
        }
        match self.mode {
            Mode::Sse => self.observe_sse(chunk),
            Mode::Json => {
                if self.held.len() + chunk.len() > MAX_JSON_BODY {
                    self.give_up("応答が大きすぎます");
                    return;
                }
                self.held.extend_from_slice(chunk);
            }
        }
    }

    /// 途中まで流れた応答は、そこまでに読めた分を返す。
    ///
    /// `message_start` まで届いていれば input は分かる。中断した分を丸ごと
    /// 捨てると、実際に消費した入力が記録から消える。
    fn finish(mut self: Box<Self>) -> Option<TokenUsage> {
        if self.given_up {
            return None;
        }
        match self.mode {
            // ストリームでない応答は、ここで初めて全体が揃う。
            Mode::Json => {
                if !self.held.is_empty()
                    && let Ok(body) = serde_json::from_slice::<Value>(&self.held)
                    && let Some(usage) = body.pointer("/usage")
                {
                    self.absorb(usage);
                }
            }
            // 終端が空行で閉じられていなければ、最後のイベントが溜まったまま
            // 残る。書きかけの行も最後の 1 行として扱う。
            Mode::Sse => {
                let last = std::mem::take(&mut self.held);
                if !last.is_empty() {
                    self.read_sse_line(&last);
                }
                self.finish_event();
            }
        }
        (!self.usage.is_empty()).then_some(self.usage)
    }
}

/// `needle` を含むか。
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        Headers::new(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    fn read(content_type: &str, chunks: &[&[u8]]) -> Option<TokenUsage> {
        let mut observer = AnthropicMetering
            .usage_observer(Some(content_type))
            .expect("読める形");
        for chunk in chunks {
            observer.observe(chunk);
        }
        observer.finish()
    }

    /// 累積で届く usage は、最後に見た値が残る。
    #[test]
    fn reads_usage_from_a_streamed_response() {
        let usage = read(
            "text/event-stream",
            &[
                b"event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":3}}}\n\n",
                b"event: message_delta\ndata: {\"usage\":{\"output_tokens\":7}}\n\n",
            ],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::input()), Some(10));
        assert_eq!(usage.get(&TokenKind::input_cache_read()), Some(3));
        assert_eq!(usage.get(&TokenKind::output()), Some(7));
    }

    /// チャンクの境目が行の途中に落ちても取りこぼさない。
    #[test]
    fn survives_chunk_boundaries_inside_a_line() {
        let usage = read(
            "text/event-stream",
            &[b"data: {\"usage\":{\"input_", b"tokens\":42}}", b"\n\n"],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::input()), Some(42));
    }

    /// 1 イベントの data が複数行に割れていても 1 つの中身として読む。
    #[test]
    fn joins_multi_line_event_data() {
        let usage = read(
            "text/event-stream",
            &[b"data: {\"usage\":\ndata: {\"output_tokens\":5}}\n\n"],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::output()), Some(5));
    }

    /// 空行で閉じられないまま終わっても、溜めた分を読む。
    #[test]
    fn reads_the_last_event_without_a_closing_blank_line() {
        let usage = read(
            "text/event-stream",
            &[b"data: {\"usage\":{\"output_tokens\":9}}"],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::output()), Some(9));
    }

    /// ストリームでない応答は終端で全体を読む。
    #[test]
    fn reads_usage_from_a_whole_json_body() {
        let usage = read(
            "application/json; charset=utf-8",
            &[br#"{"usage":{"input_tokens":1,"cache_creation_input_tokens":2}}"#],
        )
        .expect("読める");

        assert_eq!(usage.get(&TokenKind::input()), Some(1));
        assert_eq!(usage.get(&TokenKind::input_cache_creation()), Some(2));
    }

    /// usage を載せない content-type には observer を作らない。
    #[test]
    fn refuses_to_read_unknown_content_types() {
        assert!(AnthropicMetering.usage_observer(None).is_none());
        assert!(
            AnthropicMetering
                .usage_observer(Some("image/png"))
                .is_none()
        );
    }

    /// 1 つも読めなければ「観測なし」を返す (0 と区別する)。
    #[test]
    fn reports_nothing_when_no_usage_appeared() {
        assert_eq!(
            read("text/event-stream", &[b"data: {\"type\":\"ping\"}\n\n"]),
            None
        );
    }

    /// 上限を超えた応答は、途中まで読めていても捨てる。
    #[test]
    fn gives_up_on_an_endless_event() {
        let flood = vec![b'x'; MAX_SSE_EVENT + 1];
        assert_eq!(read("text/event-stream", &[&flood]), None);
    }

    /// 枠ヘッダはスナップショットへ、断られ方は締め出しの印へ写す。
    #[test]
    fn maps_quota_headers_and_rejections() {
        let quota = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ]);
        let snapshot = AnthropicMetering
            .quota_snapshot(&quota, 1_700_000_000)
            .expect("読める");
        assert_eq!(snapshot.five_hour.and_then(|w| w.utilization), Some(0.42));

        assert!(
            AnthropicMetering
                .rejection(429, &headers(&[("retry-after", "30")]), "m", 100)
                .is_some()
        );
        assert!(
            AnthropicMetering
                .rejection(200, &Headers::default(), "m", 100)
                .is_none(),
            "通った応答は締め出さない"
        );
    }

    /// 単価表にあるモデルは 4 区分に値が付き、無いモデルは値付けしない。
    #[test]
    fn prices_only_known_models() {
        let pricing = AnthropicMetering
            .pricing("claude-opus-5")
            .expect("表にある");
        for kind in [
            TokenKind::input(),
            TokenKind::output(),
            TokenKind::input_cache_creation(),
            TokenKind::input_cache_read(),
        ] {
            assert!(pricing.rates.contains_key(&kind), "{kind} の単価がない");
        }

        assert!(AnthropicMetering.pricing("no-such-model").is_none());
    }
}
