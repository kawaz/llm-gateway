//! 1 本の request を、その時に動いている系列へ書き分ける。
//!
//! 中継の経路から呼ばれる所なので、ここでするのは時刻を読む・容量を atomic に
//! 先加算する・値の束を書き手の列へ置く、だけ (reference `model` の「本体を
//! 阻害しない」)。JSON にするのも、follow へ配るのも書き手のスレッド。本文は
//! 複製 ([`Bytes`] の参照) で、中継が流すバイト列には触らない。
//!
//! 書くもの (reference `files`):
//!
//! - ヘッダを受けて ns 認証を通した時点 ([`super::Dumps::capture_head`]):
//!   `request_headers`
//! - 本文を受け取り終えた時点 ([`Capture::body`]): `*.request.json`、
//!   `*.request.body`、`request_body_end` (読めなかった本文も、受けたまま残す)
//! - upstream へ送るたび ([`Capture::connect`] と [`Capture::tee`]):
//!   `upstream_connect` / `upstream_headers` / `upstream_body_end` / `upstream_close`
//!   と、`*.<attempt>.response.json` / `*.<attempt>.response.body`
//! - クライアントへ返す時 ([`Capture::headers_sent`] と [`Settling`]):
//!   `response_headers_sent` (`adopted`) / `response_body_end` / `transport_close`

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use serde_json::{Map, Value};

use super::Series;
use super::record::{Event, Kind, RequestMeta, ResponseMeta, header};
use super::writer::Message;
use crate::credential::time::now_unix_ms;
use crate::egress::{BodyStream, Headers};

/// ヘッダを受け取り終えた 1 本 (本文はまだ)。
pub struct RequestHead {
    pub ns: String,
    /// unit の通し番号 ([`crate::exchange::request_span_numbered`] のもの)。
    pub seq: u64,
    /// ヘッダを受け取り終えた時刻 (Unix ミリ秒)。ファイル名の `{ts}` になる。
    pub received_at: i64,
    pub method: String,
    pub path: String,
    /// 受けたヘッダ (受けた順)。認証系のものは書き手が落とす。
    pub headers: Vec<(String, String)>,
}

/// 受け取り終えた本文 (gateway が書き換える前の形)。
pub struct RequestBody {
    /// 本文を受け取り終えた (または諦めた) 時刻。
    pub body_end_at: i64,
    /// 受けた本文そのまま。受け取りきれなかった時は、そこまでの分。
    pub body: Bytes,
    /// 受け取りきれなかった・読めなかった理由 (`invalid_json` / `truncated` /
    /// `too_large`)。
    pub error: Option<&'static str>,
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
    ns: Arc<str>,
    received_at: i64,
    method: Arc<str>,
    path: Arc<str>,
    headers: Arc<[(String, String)]>,
    entries: Vec<Arc<Series>>,
    /// 系列ごとに、この 1 本をまだ書くか (request を書けなかった系列は外す)。
    open: Vec<AtomicBool>,
    model: std::sync::OnceLock<Option<String>>,
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
        head: RequestHead,
        targets: Vec<Arc<Series>>,
        now: i64,
    ) -> Option<Arc<Self>> {
        let stem = format!("{}-{}", head.received_at, head.seq);
        let request_file: Arc<str> = Arc::from(format!("{stem}.request.json"));
        let method: Arc<str> = Arc::from(head.method);
        let path: Arc<str> = Arc::from(head.path);
        let first = Event {
            ts: head.received_at,
            seq: head.seq,
            kind: Kind::RequestHeaders {
                request_file,
                method: Arc::clone(&method),
                path: Arc::clone(&path),
            },
        };
        let entries: Vec<Arc<Series>> = targets
            .into_iter()
            .filter(|series| series.send(Message::Event(first.clone()), now))
            .collect();
        if entries.is_empty() {
            return None;
        }
        let open = entries.iter().map(|_| AtomicBool::new(true)).collect();
        Some(Arc::new(Self {
            seq: head.seq,
            stem,
            ns: Arc::from(head.ns),
            received_at: head.received_at,
            method,
            path,
            headers: Arc::from(head.headers),
            entries,
            open,
            model: std::sync::OnceLock::new(),
            ingress_rewrites: std::sync::Mutex::new(Map::new()),
            adopted: AtomicU64::new(0),
            headers_sent_at: AtomicI64::new(0),
        }))
    }

    /// まだ書く系列。
    fn writing(&self, now: i64) -> impl Iterator<Item = (usize, &Arc<Series>)> {
        self.entries
            .iter()
            .enumerate()
            .filter(move |(i, series)| self.open[*i].load(Ordering::Acquire) && series.live(now))
    }

    /// 本文を受け取り終えた。受けたまま書く (読めなかった本文も、何を送って
    /// きたかが調査の材料なので残す)。
    ///
    /// 書くと上限を超えるなら書かずにその系列を止める (reference `capacity`)。
    pub fn body(&self, body: RequestBody) {
        let now = now_unix_ms();
        let size = body.body.len() as u64;
        let _ = self.model.set(body.model.clone());
        let body_file: Arc<str> = Arc::from(format!("{}.request.body", self.stem));
        let meta = Arc::new(RequestMeta {
            file: Arc::from(format!("{}.request.json", self.stem)),
            seq: self.seq,
            ts: self.received_at,
            ns: Arc::clone(&self.ns),
            method: Arc::clone(&self.method),
            path: Arc::clone(&self.path),
            headers: Arc::clone(&self.headers),
            body_file: Arc::clone(&body_file),
            body_bytes: size,
            stream: body.stream,
            error: body.error,
        });
        let end = Event {
            ts: body.body_end_at,
            seq: self.seq,
            kind: Kind::RequestBodyEnd {
                bytes: size,
                duration_ms: body.body_end_at - self.received_at,
                error: body.error,
                model: body.model.as_deref().map(Arc::from),
                stream: body.stream,
            },
        };
        for (i, series) in self.writing(now) {
            let grant = series.reserve(size, true);
            if grant.bytes < size {
                self.open[i].store(false, Ordering::Release);
                if let Some(reason) = grant.limited {
                    series.stop(reason, now);
                }
                continue;
            }
            let body_sent = series.send(Message::RequestMeta(Arc::clone(&meta)), now)
                && series.send(
                    Message::Body {
                        name: Arc::clone(&body_file),
                        chunk: body.body.clone(),
                    },
                    now,
                );
            if !body_sent {
                // 書くことにしたが書き手に渡らなかった分は、未確定から外す。
                series.usage.written(size);
            }
            if !(body_sent && series.send(Message::Event(end.clone()), now)) {
                self.open[i].store(false, Ordering::Release);
            }
            if let Some(reason) = grant.limited {
                series.stop(reason, now);
            }
        }
    }

    /// 受け口が本文に加えた書き換えを覚える (試行ごとの `request_rewrites` に写す)。
    pub fn note_rewrite(&self, key: &str, value: Value) {
        super::lock(&self.ingress_rewrites).insert(key.to_owned(), value);
    }

    /// まだ書く系列へ 1 行置く。
    fn emit(&self, event: &Event, now: i64) {
        for (_, series) in self.writing(now) {
            series.send(Message::Event(event.clone()), now);
        }
    }

    /// upstream へ送り始める。`upstream_connect` を書き、その時刻を返す。
    pub fn connect(&self, attempt: u64, route: &str) -> i64 {
        let now = now_unix_ms();
        self.emit(
            &Event {
                ts: now,
                seq: self.seq,
                kind: Kind::UpstreamConnect {
                    attempt,
                    route: Arc::from(route),
                },
            },
            now,
        );
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
        let response_file: Arc<str> =
            Arc::from(format!("{}.{}.response.json", self.stem, upstream.attempt));
        let body_file: Arc<str> =
            Arc::from(format!("{}.{}.response.body", self.stem, upstream.attempt));
        let headers: Vec<(String, String)> = upstream
            .headers
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let stream = header(&headers, "content-type")
            .as_deref()
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"));
        let mut rewrites = super::lock(&self.ingress_rewrites).clone();
        let received_model = self.model.get().cloned().flatten();
        for (key, value) in upstream.rewrites {
            if key == "model" {
                if value.as_str() != received_model.as_deref() {
                    rewrites.insert(
                        key,
                        serde_json::json!({"received": received_model, "sent": value}),
                    );
                }
            } else {
                rewrites.insert(key, value);
            }
        }
        let meta = Arc::new(ResponseMeta {
            file: Arc::clone(&response_file),
            seq: self.seq,
            ts: now,
            attempt: upstream.attempt,
            route: Arc::from(upstream.route),
            credential: upstream.credential.map(Arc::from),
            status: upstream.status,
            headers,
            body_file: Arc::clone(&body_file),
            stream,
            request_rewrites: rewrites,
        });
        let line = Event {
            ts: now,
            seq: self.seq,
            kind: Kind::UpstreamHeaders {
                attempt: upstream.attempt,
                status: upstream.status,
                response_file,
                stream,
                duration_ms: now - upstream.connected_at,
            },
        };
        let mut writing: Vec<Progress> = self
            .entries
            .iter()
            .map(|_| Progress {
                bytes: 0,
                open: false,
            })
            .collect();
        for (i, series) in self.writing(now) {
            writing[i].open = series.send(Message::Event(line.clone()), now)
                && series.send(Message::ResponseMeta(Arc::clone(&meta)), now)
                && series.send(
                    Message::Open {
                        name: Arc::clone(&body_file),
                    },
                    now,
                );
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
        self.emit(
            &Event {
                ts: now,
                seq: self.seq,
                kind: Kind::ResponseHeadersSent { adopted },
            },
            now,
        );
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
    body_file: Arc<str>,
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
            let grant = series.reserve(chunk.len() as u64, false);
            let length = usize::try_from(grant.bytes).unwrap_or(chunk.len());
            if length > 0
                && !series.send(
                    Message::Body {
                        name: Arc::clone(&self.body_file),
                        chunk: chunk.slice(..length),
                    },
                    now,
                )
            {
                // 書くことにしたが書き手に渡らなかった分は、未確定から外す。
                series.usage.written(grant.bytes);
                progress.open = false;
                continue;
            }
            progress.bytes += grant.bytes;
            if let Some(reason) = grant.limited {
                // 上限に達した。この試行を打ち切りとして閉じてから系列を止める。
                series.send(
                    Message::Event(Event {
                        ts: now,
                        seq: self.capture.seq,
                        kind: Kind::UpstreamBodyEnd {
                            attempt: self.attempt,
                            bytes: progress.bytes,
                            truncated: true,
                            duration_ms: now - self.headers_at,
                            error: None,
                        },
                    }),
                    now,
                );
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
        let error: Option<Arc<str>> = error.map(Arc::from);
        for (series, progress) in self.capture.entries.iter().zip(self.writing.iter_mut()) {
            if !progress.open || !series.live(now) {
                continue;
            }
            series.send(
                Message::Event(Event {
                    ts: now,
                    seq: self.capture.seq,
                    kind: Kind::UpstreamBodyEnd {
                        attempt: self.attempt,
                        bytes: progress.bytes,
                        truncated: false,
                        duration_ms: now - self.headers_at,
                        error: error.clone(),
                    },
                }),
                now,
            );
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
        let line = Event {
            ts: now,
            seq: self.capture.seq,
            kind: Kind::UpstreamClose {
                attempt: self.attempt,
                duration_ms: now - self.ended_at.unwrap_or(self.headers_at),
            },
        };
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
        self.capture.emit(
            &Event {
                ts: now,
                seq: self.capture.seq,
                kind: Kind::ResponseBodyEnd {
                    bytes,
                    completed,
                    duration_ms: now - sent,
                },
            },
            now,
        );
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
        self.capture.emit(
            &Event {
                ts: now,
                seq: self.capture.seq,
                kind: Kind::TransportClose {
                    duration_ms: now - since,
                },
            },
            now,
        );
    }
}
