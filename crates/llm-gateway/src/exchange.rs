//! 応答本文の観測 (exchange の一部)。
//!
//! 本文は解釈せずクライアントへ素通しする。素通しするだけだと、流している
//! 途中で起きたこと (upstream が切れた / クライアントが去った) がどこにも
//! 残らない。ここで包んで、節目だけを記録する。
//!
//! 残す節目は 4 つ。本文を受け取り終えた時点 ([`record_request_body`])、
//! upstream のヘッダを受け取った時点 ([`crate::gateway`])、最初のチャンクを
//! 送り出した時点、本文の終端。並べると 1 本のリクエストの時間が
//! 「受け取り」「生成待ち」「流し切るまで」に分かれるので、途中で止まった
//! ものがどこで止まったかを切り分けられる。流している最中に詰まった分は
//! 終端の `max_gap_ms` に出る。
//!
//! ## 生成待ちはどの行に出るか
//!
//! 「最初のチャンクを送り出しました」の `elapsed_ms` に生成待ちは**入らない**。
//! 数え始めるのが upstream のヘッダを受け取った時点なので、ヘッダを返してから
//! 本文を作る upstream ならここに出るが、本文を作り終えてからヘッダを返す
//! upstream では 0 に張り付く。後者の待ちは「本文を受け取りました」と
//! 「upstream のヘッダを受け取りました」の**行の時刻差**のほうに出る。
//! どちらに出ているかは、2 つを併せて見ないと分からない。
//!
//! 節目のログは対にして読む。同じ gateway を複数のクライアントが共有するので、
//! 対にできないと「どのリクエストが落ちたか」を後から辿れない。対にするのは
//! [`request_span`] が振る `req` の番号。

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::Stream;
use tracing::field::Empty;
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
    info_span!(
        "転送",
        req = NEXT_ID.fetch_add(1, Ordering::Relaxed),
        // 受け取り終えた時点で [`record_request_body`] が埋める。
        body_bytes = Empty,
    )
}

/// クライアントから本文を受け取り終えたことを記録する。
///
/// 大きさは以降のログすべてに付く (`body_bytes`)。手前のプロキシの
/// アクセスログと 1 件ずつ突き合わせるための目印で、中断したものが
/// 大きい本文に偏っていないかもここで見る。
///
/// 一緒に残す `elapsed_ms` は受け取りにかかった時間。細い経路を通って
/// くると受信そのもので待たされるので、その分をこの先の時間と分けて
/// 読めるようにする。
pub fn record_request_body(span: &Span, bytes: usize, elapsed: Duration) {
    span.record("body_bytes", bytes as u64);
    let ms = elapsed.as_millis();
    span.in_scope(|| info!(elapsed_ms = ms, "本文を受け取りました"));
}

/// 転送するストリームを包んで、終端を記録できるようにする。
///
/// `span` は [`request_span`] が返したもの。転送したバイト数と、ヘッダを
/// 受け取ってから終端までの時間、途中で最も長く黙り込んだ時間を一緒に残す。
pub fn observe(body: BodyStream, span: Span) -> BodyObservation {
    BodyObservation {
        inner: body,
        span,
        bytes: 0,
        since_headers: Instant::now(),
        last_chunk: None,
        max_gap: Duration::ZERO,
        max_gap_at: Duration::ZERO,
        settled: false,
    }
}

/// 終端を記録するストリーム。
///
/// SSE のバイト列はここを 1 チャンクずつ通る。通り道でやるのは今の時刻を
/// 1 度見て、バイト数を足して、無音が最長かを比べるだけ。ログを書くのは
/// 節目 (最初のチャンク、終端) に限る。
pub struct BodyObservation {
    inner: BodyStream,
    span: Span,
    bytes: u64,
    /// ヘッダを受け取った時点。包んだ瞬間がそれにあたる。
    since_headers: Instant,
    /// 直前のチャンクを通した時点。まだ 1 つも通していなければ `None`。
    last_chunk: Option<Instant>,
    /// チャンクが届かなかった時間のうち、最も長かったもの。
    max_gap: Duration,
    /// その無音が始まった時点 (ヘッダ受信からの経過)。序盤で詰まったのか
    /// 終盤なのかで、疑う場所が変わる。
    max_gap_at: Duration,
    /// 終端を記録済みか。記録しないまま捨てられたら中断とみなす。
    settled: bool,
}

impl BodyObservation {
    fn elapsed_ms(&self) -> u128 {
        self.since_headers.elapsed().as_millis()
    }

    /// `at` で終わる無音を、これまでで最長なら覚えておく。
    ///
    /// 数えるのはチャンクとチャンクの間だけでなく、**最後のチャンクから
    /// 終端まで**も含む。途中まで流れてから黙り込んで切られたものは、
    /// そこが最も長い無音になる — 停滞を探すのに一番効くのはこの区間。
    ///
    /// ヘッダを受け取ってから最初のチャンクまでは数えない。そこは upstream が
    /// 返し始めるまでの待ちで、転送の停滞とは別物 (「最初のチャンクを送り
    /// 出しました」の行で見る)。
    fn note_gap(&mut self, at: Instant) {
        let Some(prev) = self.last_chunk else {
            return;
        };
        let gap = at.saturating_duration_since(prev);
        if gap > self.max_gap {
            self.max_gap = gap;
            self.max_gap_at = prev.saturating_duration_since(self.since_headers);
        }
    }

    /// 終端のログに載せる値をまとめて取る。
    ///
    /// 最後の無音をここで数え切る。チャンクが 1 つも来ていなければ無音は
    /// 存在しないので、`max_gap` は 0 のまま (`bytes` が 0 かどうかで
    /// 「1 つも来なかった」と「詰まらず流れ切った」を見分けられる)。
    fn settle(&mut self) -> (u64, u128, u128, u128) {
        self.note_gap(Instant::now());
        (
            self.bytes,
            self.elapsed_ms(),
            self.max_gap.as_millis(),
            self.max_gap_at.as_millis(),
        )
    }
}

impl Stream for BodyObservation {
    type Item = Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 中身は Pin<Box<..>> なので、包んだ側を動かしても差し支えない。
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let now = Instant::now();
                if this.last_chunk.is_none() {
                    // upstream が最初のバイトを返すまでが生成の待ち時間、
                    // ここから終端までが流し切るまでの時間。境目はここ。
                    let ms = this.elapsed_ms();
                    this.span
                        .in_scope(|| info!(elapsed_ms = ms, "最初のチャンクを送り出しました"));
                }
                this.note_gap(now);
                this.last_chunk = Some(now);
                this.bytes += chunk.len() as u64;
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.settled = true;
                let (bytes, ms, gap, gap_at) = this.settle();
                this.span.in_scope(|| {
                    error!(
                        bytes,
                        elapsed_ms = ms,
                        max_gap_ms = gap,
                        max_gap_at_ms = gap_at,
                        reason = %e,
                        "転送が途切れました"
                    );
                });
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.settled = true;
                let (bytes, ms, gap, gap_at) = this.settle();
                this.span.in_scope(|| {
                    info!(
                        bytes,
                        elapsed_ms = ms,
                        max_gap_ms = gap,
                        max_gap_at_ms = gap_at,
                        "本文を転送し終えました"
                    );
                });
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for BodyObservation {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // 終端まで読まれずに捨てられた。クライアントが去った場合がこれに
        // あたる (返す先が消えたので、サーバはストリームを落とす)。
        // 去る直前の無音がそのまま `max_gap_ms` に出る — 黙り込んだのを見て
        // 切ったのなら、その長さがここに残る。
        let (bytes, ms, gap, gap_at) = self.settle();
        self.span.in_scope(|| {
            warn!(
                bytes,
                elapsed_ms = ms,
                max_gap_ms = gap,
                max_gap_at_ms = gap_at,
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
    /// 時計を使えるようにしてあるのは、無音を作る試験があるため。
    fn logs_of<F: Future<Output = ()>>(run: impl FnOnce() -> F) -> String {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap()
                .block_on(run());
        });
        buffer.text()
    }

    fn body(chunks: Vec<Result<Bytes>>) -> BodyStream {
        futures_util::stream::iter(chunks).boxed()
    }

    /// チャンクの前に間を空けて流すストリーム。`(空ける ms, 中身)`。
    fn paused_body(chunks: Vec<(u64, &'static [u8])>) -> BodyStream {
        futures_util::stream::iter(chunks)
            .then(|(pause_ms, bytes)| async move {
                tokio::time::sleep(Duration::from_millis(pause_ms)).await;
                Ok(Bytes::from_static(bytes))
            })
            .boxed()
    }

    /// ログの行から `名前=数値` を取り出す。
    fn field(line: &str, name: &str) -> u128 {
        let rest = line
            .split_once(&format!("{name}="))
            .unwrap_or_else(|| panic!("{name} が無い: {line}"))
            .1;
        rest.split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{name} が数値でない: {line}"))
    }

    /// 終端の行を 1 つ取り出す。
    fn line_with<'a>(logs: &'a str, needle: &str) -> &'a str {
        logs.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` の行が無い:\n{logs}"))
    }

    /// 最後まで流れたら、バイト数と一緒に残る。
    #[test]
    fn records_completion_with_byte_count() {
        let logs = logs_of(|| async {
            let mut obs = observe(
                body(vec![
                    Ok(Bytes::from_static(b"event: message_start\n")),
                    Ok(Bytes::from_static(b"data: {}\n")),
                ]),
                request_span(),
            );
            while obs.next().await.is_some() {}
        });

        assert!(logs.contains("本文を転送し終えました"), "{logs}");
        assert!(
            logs.contains("bytes=30"),
            "転送したバイト数が分かる: {logs}"
        );
        assert!(logs.contains("elapsed_ms="), "かかった時間が分かる: {logs}");
    }

    /// 受け取った本文の大きさと、受け取りにかかった時間が残る。
    #[test]
    fn records_the_size_of_the_request_body() {
        let logs = logs_of(|| async {
            let span = request_span();
            record_request_body(&span, 1_300_000, Duration::from_millis(42));
        });

        assert!(logs.contains("本文を受け取りました"), "{logs}");
        assert!(
            logs.contains("body_bytes=1300000"),
            "どれだけ受け取ったか: {logs}"
        );
        assert!(
            logs.contains("elapsed_ms=42"),
            "受け取りにかかった時間が分かる: {logs}"
        );
    }

    /// 本文の大きさは、後から出るログにも付いて回る。
    /// 手前のアクセスログと 1 件ずつ突き合わせるための目印なので、
    /// 終端のログだけを見ても分かる必要がある。
    #[test]
    fn the_body_size_stays_on_later_logs() {
        let logs = logs_of(|| async {
            let span = request_span();
            record_request_body(&span, 512, Duration::from_millis(1));
            let mut obs = observe(body(vec![Ok(Bytes::from_static(b"x"))]), span);
            while obs.next().await.is_some() {}
        });

        let end = logs
            .lines()
            .find(|l| l.contains("本文を転送し終えました"))
            .unwrap_or_else(|| panic!("終端の記録が無い:\n{logs}"));
        assert!(end.contains("body_bytes=512"), "{end}");
    }

    /// 最初のチャンクを送り出した時点が残る。
    /// upstream が返し始めるまでの待ちと、流し切るまでの時間を分けて読む。
    #[test]
    fn records_when_the_first_chunk_goes_out() {
        let logs = logs_of(|| async {
            let mut obs = observe(
                body(vec![
                    Ok(Bytes::from_static(b"event: message_start\n")),
                    Ok(Bytes::from_static(b"data: {}\n")),
                    Ok(Bytes::from_static(b"data: {}\n")),
                ]),
                request_span(),
            );
            while obs.next().await.is_some() {}
        });

        let first: Vec<&str> = logs
            .lines()
            .filter(|l| l.contains("最初のチャンクを送り出しました"))
            .collect();
        assert_eq!(first.len(), 1, "残すのは最初の 1 回だけ: {logs}");
        assert!(first[0].contains("elapsed_ms="), "{}", first[0]);
    }

    /// 1 バイトも来ないまま終わったら、送り出した記録は残らない。
    /// 「生成待ちのまま終わった」と「流している途中で終わった」は別物。
    #[test]
    fn nothing_is_recorded_when_no_chunk_arrives() {
        let logs = logs_of(|| async {
            let mut obs = observe(
                body(vec![Err(Error::Config(
                    "応答の読み取りが途切れました".into(),
                ))]),
                request_span(),
            );
            let _ = obs.next().await;
        });

        assert!(!logs.contains("最初のチャンク"), "{logs}");
        assert!(logs.contains("転送が途切れました"), "終端は残る: {logs}");
    }

    /// チャンクの間で最も長く黙り込んだ時間が残る。
    ///
    /// 平均で見ると詰まりは均されて消える。知りたいのは「途中で何秒
    /// 詰まったか」なので、最長の 1 回を残す。
    #[test]
    fn records_the_longest_silence_between_chunks() {
        let logs = logs_of(|| async {
            let mut obs = observe(
                paused_body(vec![
                    (0, b"data: 1\n"),
                    (5, b"data: 2\n"),
                    // ここが最長。
                    (80, b"data: 3\n"),
                    (5, b"data: 4\n"),
                ]),
                request_span(),
            );
            while obs.next().await.is_some() {}
        });

        let end = line_with(&logs, "本文を転送し終えました");
        let gap = field(end, "max_gap_ms");
        assert!(gap >= 60, "最長の無音を拾う (80ms 空けた): {end}");

        // 3 つ目の前で詰まったので、無音の始まりは 1 つ目と 2 つ目の分だけ後。
        let at = field(end, "max_gap_at_ms");
        assert!(at < gap, "詰まったのが序盤だと分かる (at={at}, gap={gap})");
    }

    /// 流れている途中で黙り込んだまま切られたら、その無音が残る。
    ///
    /// クライアントが停滞を見て切った場合がこれ。最後のチャンクから
    /// 中断までを数えないと、まさに知りたい区間が抜ける。
    #[test]
    fn silence_before_an_abort_is_counted() {
        let logs = logs_of(|| async {
            let mut obs = observe(
                paused_body(vec![(0, b"data: 1\n"), (0, b"data: 2\n")]),
                request_span(),
            );
            let _ = obs.next().await;
            // 受け取ったきり黙り込んで、捨てる。
            tokio::time::sleep(Duration::from_millis(80)).await;
        });

        let aborted = line_with(&logs, "転送が中断されました");
        assert!(
            field(aborted, "max_gap_ms") >= 60,
            "去る直前の無音が残る: {aborted}"
        );
    }

    /// チャンクが 1 つも来なければ、無音は 0。
    ///
    /// 数えるのはチャンクとチャンクの間なので、1 つも来ていなければ
    /// 区間そのものが無い。生成待ちのまま終わったことは `bytes=0` と
    /// 「最初のチャンク」の行が無いことで分かる。
    #[test]
    fn no_silence_is_counted_when_no_chunk_arrives() {
        let logs = logs_of(|| async {
            let mut obs = observe(
                body(vec![Err(Error::Config(
                    "応答の読み取りが途切れました".into(),
                ))]),
                request_span(),
            );
            let _ = obs.next().await;
        });

        let broken = line_with(&logs, "転送が途切れました");
        assert_eq!(field(broken, "max_gap_ms"), 0, "{broken}");
        assert_eq!(field(broken, "bytes"), 0, "{broken}");
    }

    /// 途中で upstream が切れたら、切れたと分かる形で残る。
    #[test]
    fn records_failure_midway() {
        let logs = logs_of(|| async {
            let mut obs = observe(
                body(vec![
                    Ok(Bytes::from_static(b"data: {}\n")),
                    Err(Error::Config("応答の読み取りが途切れました".into())),
                ]),
                request_span(),
            );
            while let Some(item) = obs.next().await {
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
            let mut obs = observe(
                body(vec![
                    Ok(Bytes::from_static(b"data: {}\n")),
                    Ok(Bytes::from_static(b"data: {}\n")),
                ]),
                request_span(),
            );
            // 1 つだけ受け取って捨てる。
            let _ = obs.next().await;
        });

        assert!(logs.contains("転送が中断されました"), "{logs}");
        assert!(logs.contains("bytes=9"), "どこまで流したか: {logs}");
    }

    /// 終端まで流したものを捨てても、中断にはしない。
    #[test]
    fn completed_stream_is_not_reported_as_aborted() {
        let logs = logs_of(|| async {
            let mut obs = observe(body(vec![Ok(Bytes::from_static(b"x"))]), request_span());
            while obs.next().await.is_some() {}
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
