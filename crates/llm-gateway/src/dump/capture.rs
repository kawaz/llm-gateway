//! 1 本の request を、その時に動いている系列へ書き分ける。
//!
//! 中継の経路から呼ばれる所なので、ここでするのは時刻を読む・容量を atomic に
//! 先加算する・書き手の列へ置く、だけ (reference `model` の「本体を阻害しない」)。
//! 本文は複製 ([`Bytes`] の参照) で、中継が流すバイト列には触らない。
//!
//! 書くもの (reference `files`):
//!
//! - 受けた時点: `request_headers` / `request_body_end` の行、`*.request.json` と
//!   `*.request.body`
//! - upstream へ送るたび ([`Capture::connect`] と [`Capture::tee`]):
//!   `upstream_connect` / `upstream_headers` / `upstream_body_end` / `upstream_close`
//!   の行、`*.<attempt>.response.json` と `*.<attempt>.response.body`
//! - クライアントへ返す時 ([`Capture::headers_sent`] と [`Settling`]):
//!   `response_headers_sent` (`adopted`) / `response_body_end` / `transport_close`

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use serde_json::{Map, Value, json};

use super::Series;
use super::writer::Message;
use crate::credential::time::now_unix_ms;
use crate::egress::{BodyStream, Headers};

/// 受け口が受けた 1 本 (gateway が書き換える前の形)。
pub struct Received {
    pub ns: String,
    /// unit の通し番号 ([`crate::exchange::request_span_numbered`] のもの)。
    pub seq: u64,
    /// ヘッダを受け取り終えた時刻 (Unix ミリ秒)。ファイル名の `{ts}` になる。
    pub received_at: i64,
    /// 本文を受け取り終えた時刻。
    pub body_end_at: i64,
    pub method: String,
    pub path: String,
    /// 受けたヘッダ (受けた順)。認証系のものはここで落とす。
    pub headers: Vec<(String, String)>,
    /// 受けた本文そのまま。
    pub body: Bytes,
    /// クライアントが `stream` を求めたか。
    pub stream: bool,
    /// クライアントが名乗ったモデル名 (別名の解決前)。
    pub model: Option<String>,
}

/// upstream が答えた 1 回の試行。
pub struct Upstream<'a> {
    /// gateway の送信ループが振る番号 (1 から)。
    pub attempt: u64,
    pub route: &'a str,
    pub credential: Option<&'a str>,
    pub status: u16,
    pub headers: &'a Headers,
    /// [`Capture::connect`] が返した時刻。
    pub connected_at: i64,
    /// この経路へ送る形を決めるまでに gateway が本文に加えた書き換え。
    pub rewrites: Map<String, Value>,
}

/// 受けた 1 本と、それを書く系列。
pub struct Capture {
    seq: u64,
    stem: String,
    model: Option<String>,
    entries: Vec<Arc<Series>>,
    /// 受け口が加えた書き換え (thinking の display)。試行ごとの
    /// `request_rewrites` に写す。
    ingress_rewrites: std::sync::Mutex<Map<String, Value>>,
    /// クライアントへ返した試行。0 は upstream の応答を返していない。
    adopted: AtomicU64,
    /// クライアントへヘッダを返した時刻。0 はまだ。
    headers_sent_at: AtomicI64,
}

impl Capture {
    pub(super) fn begin(
        received: Received,
        targets: Vec<Arc<Series>>,
        now: i64,
    ) -> Option<Arc<Self>> {
        let stem = format!("{}-{}", received.received_at, received.seq);
        let request_file = format!("{stem}.request.json");
        let body_file = format!("{stem}.request.body");
        let size = received.body.len() as u64;
        let content_type = header(&received.headers, "content-type");
        let content_encoding = header(&received.headers, "content-encoding");
        let headers = kept_headers(&received.headers);

        let mut entries = Vec::new();
        for series in targets {
            // 書くと上限を超えるなら書かずに止まる (reference `capacity`)。
            let grant = series.reserve(size, true);
            if grant.bytes < size {
                if let Some(reason) = grant.limited {
                    series.stop(reason, now);
                }
                continue;
            }
            let mut first = event("request_headers", received.received_at, received.seq);
            first.insert("request_file".to_owned(), json!(request_file));
            first.insert("method".to_owned(), json!(received.method));
            first.insert("path".to_owned(), json!(received.path));
            first.insert("model".to_owned(), json!(received.model));
            first.insert("stream".to_owned(), json!(received.stream));
            let meta = json!({
                "seq": received.seq,
                "ts": received.received_at,
                "ns": received.ns,
                "method": received.method,
                "path": received.path,
                "headers": headers,
                "content_type": content_type,
                "content_encoding": content_encoding,
                "stream": received.stream,
                "body_file": body_file,
                "body_stored": "raw",
                "body_bytes": size,
            });
            let mut end = event("request_body_end", received.body_end_at, received.seq);
            end.insert("bytes".to_owned(), json!(size));
            end.insert(
                "duration_ms".to_owned(),
                json!(received.body_end_at - received.received_at),
            );
            let sent = series.send(Message::Event(first), now)
                && series.send(
                    Message::Meta {
                        name: request_file.clone(),
                        json: meta,
                    },
                    now,
                )
                && series.send(
                    Message::Body {
                        name: body_file.clone(),
                        chunk: received.body.clone(),
                    },
                    now,
                )
                && series.send(Message::Event(end), now);
            if let Some(reason) = grant.limited {
                series.stop(reason, now);
            }
            if sent {
                entries.push(series);
            }
        }
        if entries.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            seq: received.seq,
            stem,
            model: received.model,
            entries,
            ingress_rewrites: std::sync::Mutex::new(Map::new()),
            adopted: AtomicU64::new(0),
            headers_sent_at: AtomicI64::new(0),
        }))
    }

    /// 受け口が本文に加えた書き換えを覚える (試行ごとの `request_rewrites` に写す)。
    pub fn note_rewrite(&self, key: &str, value: Value) {
        super::lock(&self.ingress_rewrites).insert(key.to_owned(), value);
    }

    /// 動いている系列へ 1 行置く。
    fn emit(&self, event: &Map<String, Value>, now: i64) {
        for series in &self.entries {
            if series.live(now) {
                series.send(Message::Event(event.clone()), now);
            }
        }
    }

    /// upstream へ送り始める。`upstream_connect` を書き、その時刻を返す。
    pub fn connect(&self, attempt: u64, route: &str) -> i64 {
        let now = now_unix_ms();
        let mut line = event("upstream_connect", now, self.seq);
        line.insert("attempt".to_owned(), json!(attempt));
        line.insert("route".to_owned(), json!(route));
        self.emit(&line, now);
        now
    }

    /// upstream から読んだ本文を、そのまま流しながら系列ごとのファイルに書く。
    /// Wire が方言を通訳する前、gateway が本文を集め直したりエラーの本文に
    /// 差し替えたりする前のバイト列。
    ///
    /// 試行ごとに別のファイルに書き、前の試行を消さず量からも差し引かない
    /// (gateway は先に断られた応答を持ったまま次を試し、後の応答を捨てて先の
    /// 応答を返すことがある)。返した試行は gateway が [`Self::adopt`] で伝える。
    pub fn tee(self: &Arc<Self>, upstream: Upstream<'_>, body: BodyStream) -> BodyStream {
        let now = now_unix_ms();
        let response_file = format!("{}.{}.response.json", self.stem, upstream.attempt);
        let body_file = format!("{}.{}.response.body", self.stem, upstream.attempt);
        let pairs: Vec<(String, String)> = upstream
            .headers
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let content_type = header(&pairs, "content-type");
        let stream = content_type
            .as_deref()
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"));
        let mut rewrites = super::lock(&self.ingress_rewrites).clone();
        if let Some(Value::String(sent)) = upstream.rewrites.get("model")
            && self.model.as_deref() != Some(sent.as_str())
        {
            rewrites.insert(
                "model".to_owned(),
                json!({"received": self.model, "sent": sent}),
            );
        }
        for (key, value) in upstream.rewrites {
            if key != "model" {
                rewrites.insert(key, value);
            }
        }
        let meta = json!({
            "seq": self.seq,
            "ts": now,
            "attempt": upstream.attempt,
            "route": upstream.route,
            "credential": upstream.credential,
            "status": upstream.status,
            "headers": kept_headers(&pairs),
            "content_type": content_type,
            "content_encoding": header(&pairs, "content-encoding"),
            "stream": stream,
            "body_file": body_file,
            "body_stored": "raw",
            "request_rewrites": rewrites,
        });
        let mut line = event("upstream_headers", now, self.seq);
        line.insert("attempt".to_owned(), json!(upstream.attempt));
        line.insert("status".to_owned(), json!(upstream.status));
        line.insert("response_file".to_owned(), json!(response_file));
        line.insert("stream".to_owned(), json!(stream));
        line.insert("duration_ms".to_owned(), json!(now - upstream.connected_at));
        let mut writing = Vec::new();
        for series in &self.entries {
            let open = series.live(now)
                && series.send(Message::Event(line.clone()), now)
                && series.send(
                    Message::Meta {
                        name: response_file.clone(),
                        json: meta.clone(),
                    },
                    now,
                )
                && series.send(
                    Message::Open {
                        name: body_file.clone(),
                    },
                    now,
                );
            writing.push(Progress { bytes: 0, open });
        }
        Box::pin(Teed {
            inner: body,
            capture: Arc::clone(self),
            attempt: upstream.attempt,
            body_file,
            headers_at: now,
            ended_at: None,
            writing,
        })
    }

    /// gateway がクライアントへ返すと決めた試行。
    pub fn adopt(&self, attempt: u64) {
        self.adopted.store(attempt, Ordering::Release);
    }

    /// クライアントへヘッダを返す。`adopted` は返した試行、gateway が自分で組んだ
    /// 応答なら `null`。
    pub fn headers_sent(&self) {
        let now = now_unix_ms();
        self.headers_sent_at.store(now, Ordering::Release);
        let adopted = match self.adopted.load(Ordering::Acquire) {
            0 => None,
            attempt => Some(attempt),
        };
        let mut line = event("response_headers_sent", now, self.seq);
        line.insert("adopted".to_owned(), json!(adopted));
        self.emit(&line, now);
    }

    /// クライアントへ流す本文の終わりを [`Settling`] に任せる。
    pub fn settling(self: Arc<Self>) -> Settling {
        Settling {
            capture: self,
            body_end_at: AtomicI64::new(0),
        }
    }
}

/// 1 系列への、1 試行の本文の書き込み具合。
struct Progress {
    bytes: u64,
    /// まだ書くか。止まった・上限で打ち切った系列は `false`。
    open: bool,
}

/// upstream の本文を、流しながら書き手へ渡すストリーム。
struct Teed {
    inner: BodyStream,
    capture: Arc<Capture>,
    attempt: u64,
    body_file: String,
    headers_at: i64,
    ended_at: Option<i64>,
    writing: Vec<Progress>,
}

impl Teed {
    fn chunk(&mut self, chunk: &Bytes) {
        let now = now_unix_ms();
        for (series, progress) in self.capture.entries.iter().zip(self.writing.iter_mut()) {
            if !progress.open {
                continue;
            }
            // 止まった系列には、流れている途中の応答も書かない。
            if !series.live(now) {
                progress.open = false;
                continue;
            }
            let want = chunk.len() as u64;
            let grant = series.reserve(want, false);
            let length = usize::try_from(grant.bytes).unwrap_or(chunk.len());
            if length > 0
                && !series.send(
                    Message::Body {
                        name: self.body_file.clone(),
                        chunk: chunk.slice(..length),
                    },
                    now,
                )
            {
                progress.open = false;
                continue;
            }
            progress.bytes += grant.bytes;
            if let Some(reason) = grant.limited {
                // 上限に達した。この試行を打ち切りとして閉じてから系列を止める。
                let mut line = event("upstream_body_end", now, self.capture.seq);
                line.insert("attempt".to_owned(), json!(self.attempt));
                line.insert("bytes".to_owned(), json!(progress.bytes));
                line.insert("truncated".to_owned(), json!(true));
                line.insert("duration_ms".to_owned(), json!(now - self.headers_at));
                series.send(Message::Event(line), now);
                series.stop(reason, now);
                progress.open = false;
            }
        }
    }

    /// upstream の本文が終わった (途切れたなら `error` を添える)。
    fn end(&mut self, error: Option<String>) {
        if self.ended_at.is_some() {
            return;
        }
        let now = now_unix_ms();
        self.ended_at = Some(now);
        for (series, progress) in self.capture.entries.iter().zip(self.writing.iter_mut()) {
            if !progress.open || !series.live(now) {
                continue;
            }
            let mut line = event("upstream_body_end", now, self.capture.seq);
            line.insert("attempt".to_owned(), json!(self.attempt));
            line.insert("bytes".to_owned(), json!(progress.bytes));
            line.insert("truncated".to_owned(), json!(false));
            line.insert("duration_ms".to_owned(), json!(now - self.headers_at));
            if let Some(error) = &error {
                line.insert("error".to_owned(), json!(error));
            }
            series.send(Message::Event(line), now);
        }
    }
}

impl Stream for Teed {
    type Item = crate::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 中身は Pin<Box<..>> なので、包んだ側を動かしても差し支えない。
        let this = self.get_mut();
        let polled = this.inner.as_mut().poll_next(cx);
        match &polled {
            Poll::Ready(Some(Ok(chunk))) => this.chunk(chunk),
            Poll::Ready(Some(Err(e))) => this.end(Some(e.to_string())),
            Poll::Ready(None) => this.end(None),
            Poll::Pending => {}
        }
        polled
    }
}

impl Drop for Teed {
    /// upstream との繋がりを手放した。本文を読み切らずに捨てられた試行
    /// (gateway が後の応答を捨てた等) は `upstream_body_end` を持たない。
    fn drop(&mut self) {
        let now = now_unix_ms();
        let mut line = event("upstream_close", now, self.capture.seq);
        line.insert("attempt".to_owned(), json!(self.attempt));
        line.insert(
            "duration_ms".to_owned(),
            json!(now - self.ended_at.unwrap_or(self.headers_at)),
        );
        for (series, progress) in self.capture.entries.iter().zip(&self.writing) {
            if progress.open && series.live(now) {
                series.send(Message::Event(line.clone()), now);
            }
        }
    }
}

/// クライアントへ本文を流し終える (または切れる) のを待っている 1 本。
pub struct Settling {
    capture: Arc<Capture>,
    body_end_at: AtomicI64,
}

impl Settling {
    /// クライアントへ本文を流し終えた (`completed` が偽なら途中で切れた)。
    pub fn body_end(&self, bytes: u64, completed: bool) {
        let now = now_unix_ms();
        self.body_end_at.store(now, Ordering::Release);
        let sent = self.capture.headers_sent_at.load(Ordering::Acquire);
        let mut line = event("response_body_end", now, self.capture.seq);
        line.insert("bytes".to_owned(), json!(bytes));
        line.insert("completed".to_owned(), json!(completed));
        line.insert("duration_ms".to_owned(), json!(now - sent));
        self.capture.emit(&line, now);
    }
}

impl Drop for Settling {
    /// クライアントとの繋がりを手放した。
    fn drop(&mut self) {
        let now = now_unix_ms();
        let since = match self.body_end_at.load(Ordering::Acquire) {
            0 => self.capture.headers_sent_at.load(Ordering::Acquire),
            at => at,
        };
        let mut line = event("transport_close", now, self.capture.seq);
        line.insert("duration_ms".to_owned(), json!(now - since));
        self.capture.emit(&line, now);
    }
}

/// `event` / `ts` / `seq` を持つ行の書き出し。
fn event(name: &str, ts: i64, seq: u64) -> Map<String, Value> {
    let mut line = Map::new();
    line.insert("event".to_owned(), json!(name));
    line.insert("ts".to_owned(), json!(ts));
    line.insert("seq".to_owned(), json!(seq));
    line
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// メタ json に書くヘッダ: 受けた順の `{name, value}` の配列。同名を繋がない。
/// 認証系 (keepalive の控えと同じ述語) だけを落とす。
fn kept_headers(headers: &[(String, String)]) -> Vec<Value> {
    headers
        .iter()
        .filter(|(name, _)| !crate::gateway::is_authorization(name))
        .map(|(name, value)| json!({"name": name.to_ascii_lowercase(), "value": value}))
        .collect()
}
