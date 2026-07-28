//! 中継の観測。
//!
//! 本文は解釈せずクライアントへ素通しする。素通しするだけだと、流している
//! 途中で起きたこと (upstream が切れた / クライアントが去った) がどこにも
//! 残らない。ここで包んで、終端だけを記録する。
//!
//! ヘッダを受け取った時点のログ ([`crate::gateway`]) と、本文を流し終えた
//! 時点のログは対にして読む。同じ gateway を複数のクライアントが共有するので、
//! 対にできないと「どのリクエストが落ちたか」を後から辿れない。対にするのは
//! [`request_span`] が振る `req` の番号。

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use futures_util::Stream;
use tracing::{Span, error, info, info_span, warn};

use crate::Result;
use crate::backend::anthropic::forward::BodyStream;

/// リクエストに振る通し番号。
///
/// プロセス内で重複しなければ用は足りる (ログには時刻も並ぶ)。
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// リクエスト 1 本を追うための span を作る。
///
/// 転送の間に出るログはこの span の下に入る。`req` の番号で、開始
/// (ヘッダ受信) と終了 (本文の終端) を突き合わせられる。
pub fn request_span() -> Span {
    info_span!("転送", req = NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

/// 中継するストリームを包んで、終端を記録できるようにする。
///
/// `span` は [`request_span`] が返したもの。転送したバイト数と、ヘッダを
/// 受け取ってから終端までの時間を一緒に残す。
pub fn observe(body: BodyStream, span: Span) -> Relay {
    Relay {
        inner: body,
        span,
        bytes: 0,
        since_headers: Instant::now(),
        settled: false,
    }
}

/// 終端を記録するストリーム。
///
/// SSE のバイト列はここを 1 チャンクずつ通る。通り道でやるのはバイト数の
/// 加算だけにしてある。記録するのは終端に達したときの 1 回。
pub struct Relay {
    inner: BodyStream,
    span: Span,
    bytes: u64,
    /// ヘッダを受け取った時点。包んだ瞬間がそれにあたる。
    since_headers: Instant,
    /// 終端を記録済みか。記録しないまま捨てられたら中断とみなす。
    settled: bool,
}

impl Relay {
    fn elapsed_ms(&self) -> u128 {
        self.since_headers.elapsed().as_millis()
    }
}

impl Stream for Relay {
    type Item = Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 中身は Pin<Box<..>> なので、包んだ側を動かしても差し支えない。
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.bytes += chunk.len() as u64;
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.settled = true;
                let (bytes, ms) = (this.bytes, this.elapsed_ms());
                this.span.in_scope(|| {
                    error!(bytes, elapsed_ms = ms, reason = %e, "転送が途切れました");
                });
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.settled = true;
                let (bytes, ms) = (this.bytes, this.elapsed_ms());
                this.span.in_scope(|| {
                    info!(bytes, elapsed_ms = ms, "本文を転送し終えました");
                });
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // 終端まで読まれずに捨てられた。クライアントが去った場合がこれに
        // あたる (返す先が消えたので、サーバはストリームを落とす)。
        let (bytes, ms) = (self.bytes, self.elapsed_ms());
        self.span.in_scope(|| {
            warn!(
                bytes,
                elapsed_ms = ms,
                "転送が中断されました (クライアントが切れたとみられます)"
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use futures_util::StreamExt as _;
    use std::sync::{Arc, Mutex};

    /// ログを溜める書き出し先。
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// `run` の間に出たログを集める。
    ///
    /// 受け取り先はこのスレッドにだけ差し込む (他の試験と混ざらない)。
    /// 中で待つものがあるので、同じスレッドで回るランタイムを渡す。
    fn logs_of<F: Future<Output = ()>>(run: impl FnOnce() -> F) -> String {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(run());
        });
        buffer.text()
    }

    fn body(chunks: Vec<Result<Bytes>>) -> BodyStream {
        futures_util::stream::iter(chunks).boxed()
    }

    /// 最後まで流れたら、バイト数と一緒に残る。
    #[test]
    fn records_completion_with_byte_count() {
        let logs = logs_of(|| async {
            let mut relay = observe(
                body(vec![
                    Ok(Bytes::from_static(b"event: message_start\n")),
                    Ok(Bytes::from_static(b"data: {}\n")),
                ]),
                request_span(),
            );
            while relay.next().await.is_some() {}
        });

        assert!(logs.contains("本文を転送し終えました"), "{logs}");
        assert!(
            logs.contains("bytes=30"),
            "転送したバイト数が分かる: {logs}"
        );
        assert!(logs.contains("elapsed_ms="), "かかった時間が分かる: {logs}");
    }

    /// 途中で upstream が切れたら、切れたと分かる形で残る。
    #[test]
    fn records_failure_midway() {
        let logs = logs_of(|| async {
            let mut relay = observe(
                body(vec![
                    Ok(Bytes::from_static(b"data: {}\n")),
                    Err(Error::Config("応答の読み取りが途切れました".into())),
                ]),
                request_span(),
            );
            while let Some(item) = relay.next().await {
                if item.is_err() {
                    break;
                }
            }
        });

        assert!(logs.contains("転送が途切れました"), "{logs}");
        assert!(
            logs.contains("応答の読み取りが途切れました"),
            "何が起きたか: {logs}"
        );
        assert!(logs.contains("bytes=9"), "どこまで流したか: {logs}");
    }

    /// 終端まで読まれずに捨てられたら、中断として残る。
    /// クライアントが去った場合がこれ。
    #[test]
    fn records_abort_when_dropped_before_the_end() {
        let logs = logs_of(|| async {
            let mut relay = observe(
                body(vec![
                    Ok(Bytes::from_static(b"data: {}\n")),
                    Ok(Bytes::from_static(b"data: {}\n")),
                ]),
                request_span(),
            );
            // 1 つだけ受け取って捨てる。
            let _ = relay.next().await;
        });

        assert!(logs.contains("転送が中断されました"), "{logs}");
        assert!(logs.contains("bytes=9"), "どこまで流したか: {logs}");
    }

    /// 終端まで流したものを捨てても、中断にはしない。
    #[test]
    fn completed_stream_is_not_reported_as_aborted() {
        let logs = logs_of(|| async {
            let mut relay = observe(body(vec![Ok(Bytes::from_static(b"x"))]), request_span());
            while relay.next().await.is_some() {}
        });

        assert!(!logs.contains("中断"), "{logs}");
    }

    /// リクエストごとに番号が変わる。同時に流れていても取り違えない。
    #[test]
    fn each_request_gets_its_own_number() {
        let logs = logs_of(|| async {
            for _ in 0..2 {
                let span = request_span();
                span.in_scope(|| info!("しるし"));
            }
        });

        let numbers: Vec<&str> = logs
            .lines()
            .filter(|l| l.contains("しるし"))
            .filter_map(|l| l.split_once("req=").map(|(_, rest)| rest))
            .collect();
        assert_eq!(numbers.len(), 2, "{logs}");
        assert_ne!(numbers[0], numbers[1], "番号が使い回されない: {logs}");
    }
}
