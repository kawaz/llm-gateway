//! 1 転送の生涯を持つ型 (DR-0014 §1)。
//!
//! ここが観測フックの掛け先になる。持つのは 3 つ。
//!
//! - **request span** ([`request_span`]) — 1 リクエストのログをまとめる目印
//! - **節目の記録** ([`record_request_body`] / [`record_upstream_headers`]) —
//!   本文を受け取った時点・upstream のヘッダを受け取った時点というフック
//! - **応答本文の観測** ([`observe`]) — 流すバイト列はそのままに、usage を
//!   抽出する役 ([`crate::metering::UsageObserver`]) へ通しつつ、転送の
//!   終端 (完了・エラー・中断) をログへ残す
//!
//! 本文は解釈せずクライアントへ素通しする。素通しするだけだと、流している
//! 途中で起きたこと (upstream が切れた / クライアントが去った / 消費した
//! トークン数) がどこにも残らない。ここで包んで、節目と usage の両方を拾う。
//!
//! 残す節目は 4 つ。本文を受け取り終えた時点 ([`record_request_body`])、
//! upstream のヘッダを受け取った時点 ([`record_upstream_headers`])、最初の
//! チャンクを送り出した時点、本文の終端。並べると 1 本のリクエストの時間が
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
//! ## usage の抽出は本文の観測に相乗りする
//!
//! 本文を読む役 ([`crate::metering::UsageObserver`]) を作れるのは応答を
//! 出した provider だけ (DR-0014 §4)。ここは役を受け取ってチャンクが通る
//! たびに見せるだけで、中身は解釈しない。終端 (完了・エラー) または Drop
//! (中断) で役を締め、読めた usage を [`crate::stats::Stats`] へ積む。途中で
//! 切れても、そこまでに読めた分は記録する — 中断した分を丸ごと捨てると、
//! 実際に消費した入力が記録から消える。役を持たない応答 (エラー / 読めない
//! content-type) では usage を積まないだけで、節目のログは変わらず残す。
//!
//! 節目のログは対にして読む。同じ gateway を複数のクライアントが共有するので、
//! 対にできないと「どのリクエストが落ちたか」を後から辿れない。対にするのは
//! [`request_span`] が振る `req` の番号。

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::Stream;
use tracing::field::Empty;
use tracing::{Span, error, info, info_span, warn};

use crate::Result;
use crate::egress::BodyStream;
use crate::metering::UsageObserver;
use crate::stats::Stats;
use crate::tap::{self, Tap};

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
        "exchange",
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
    span.in_scope(|| info!(elapsed_ms = ms, "received request body"));
}

/// upstream からヘッダを受け取った節目を記録する。
///
/// 本文はまだ流れていない。ここで残しておくと、この後の本文終端までの
/// 時間が「生成待ち」寄りか「転送」寄りかを、この行と本文終端の行を
/// 突き合わせて読める (詳しくはモジュール冒頭の「生成待ちはどの行に出るか」)。
pub fn record_upstream_headers(span: &Span, model: &str, route: &str, status: u16) {
    span.in_scope(|| {
        info!(model = %model, route, status, "received upstream headers");
    });
}

/// 転送するストリームを包んで、usage の抽出と終端の記録を両方受け持たせる。
///
/// `span` は [`request_span`] が返したもの。`observer` は応答を出した
/// provider が作った役 ([`crate::provider::Metering`] 経由、DR-0014 §4) —
/// 読めない応答では `None` で、その場合 usage は記録されないが、節目の
/// ログは observer の有無に関わらず残す。`stats` は usage の積み先、
/// `at` / `credential` / `model` は積むときの鍵になる (`at` は応答が
/// 始まった時刻。生成が日を跨いで終わっても、始めた日に付ける)。
pub fn observe(
    body: BodyStream,
    observer: Option<Box<dyn UsageObserver>>,
    stats: Arc<Stats>,
    at: i64,
    credential: Option<&str>,
    model: &str,
    span: Span,
) -> BodyObservation {
    BodyObservation {
        inner: body,
        span,
        bytes: 0,
        since_headers: Instant::now(),
        last_chunk: None,
        max_gap: Duration::ZERO,
        max_gap_at: Duration::ZERO,
        settled: false,
        observer,
        stats,
        at,
        credential: credential.map(str::to_owned),
        model: model.to_owned(),
        tap: None,
        response_body: Vec::new(),
    }
}

/// 1 exchange の tap 配信に必要な値。
pub struct TapObservation {
    pub tap: Arc<Tap>,
    pub event: tap::Event,
    pub response_body_limit: usize,
}

/// 終端を記録し、usage を抽出しながら流すストリーム。
///
/// SSE のバイト列はここを 1 チャンクずつ通る。通り道でやるのは、今の時刻を
/// 1 度見てバイト数を足し無音の最長を比べる (節目の記録) のと、observer に
/// チャンクを見せる (usage の抽出) だけ。ログを書くのも usage を積むのも
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
    /// 応答から usage を読む役。後始末で `finish` するために `Option` で持つ
    /// ([`Drop`] は値を動かせない)。読めない応答では最初から `None`。
    observer: Option<Box<dyn UsageObserver>>,
    /// 読めた usage の積み先。
    stats: Arc<Stats>,
    /// 応答が始まった時刻。集計の日付をこれで決める。
    at: i64,
    credential: Option<String>,
    model: String,
    tap: Option<TapObservation>,
    response_body: Vec<u8>,
}

impl BodyObservation {
    /// tap 購読時だけ応答本文を上限まで控え、交換の終端で 1 件配信する。
    pub fn with_tap(mut self, tap: Option<TapObservation>) -> Self {
        self.tap = tap;
        self
    }

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

    /// 観測役を締めて、読めた usage があれば集計へ渡す。
    ///
    /// 呼ばれるのは [`Self::settle`] の中だけ — 終端 (完了・エラー) または
    /// Drop (中断) の 1 度きり。正常に終わった場合も、クライアントが去って
    /// 途中で捨てられた場合も同じ道を通る。途中まで流れた応答は、そこまでに
    /// 読めた分が入る (`message_start` まで届いていれば input は分かる) —
    /// 中断した分を丸ごと捨てると、実際に消費した入力が記録から消える。
    fn finish_usage(&mut self) {
        let Some(observer) = self.observer.take() else {
            return;
        };
        let Some(usage) = observer.finish() else {
            return;
        };
        self.stats
            .record(self.at, self.credential.as_deref(), &self.model, &usage);
    }

    /// 終端のログに載せる値をまとめて取り、usage も締める。
    ///
    /// 最後の無音をここで数え切る。チャンクが 1 つも来ていなければ無音は
    /// 存在しないので、`max_gap` は 0 のまま (`bytes` が 0 かどうかで
    /// 「1 つも来なかった」と「詰まらず流れ切った」を見分けられる)。
    fn settle(&mut self) -> (u64, u128, u128, u128) {
        self.note_gap(Instant::now());
        self.finish_usage();
        if let Some(mut observation) = self.tap.take() {
            observation.event.response_body_size = self.bytes as usize;
            observation.event.response_body =
                tap::capture(&self.response_body, observation.response_body_limit);
            observation.tap.publish(observation.event);
        }
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
                        .in_scope(|| info!(elapsed_ms = ms, "sent the first chunk"));
                }
                this.note_gap(now);
                this.last_chunk = Some(now);
                this.bytes += chunk.len() as u64;
                if let Some(observation) = this.tap.as_ref()
                    && this.response_body.len() < observation.response_body_limit
                {
                    let remaining = observation.response_body_limit - this.response_body.len();
                    this.response_body
                        .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                if let Some(observer) = this.observer.as_mut() {
                    observer.observe(&chunk);
                }
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
                        "the transfer broke off"
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
                        "finished transferring the body"
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
        // 切ったのなら、その長さがここに残る。usage もここで締める
        // ([`Self::finish_usage`]) — ここまでに読めた分は記録に残す。
        let (bytes, ms, gap, gap_at) = self.settle();
        self.span.in_scope(|| {
            warn!(
                bytes,
                elapsed_ms = ms,
                max_gap_ms = gap,
                max_gap_at_ms = gap_at,
                "the transfer was aborted (the client appears to have disconnected)"
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::metering::{TokenKind, TokenUsage};
    use crate::stats::ByDate;
    use futures_util::StreamExt as _;
    use std::sync::Mutex;

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

    /// usage を記録しない、節目ログだけを見る試験用の `observe`。
    ///
    /// 積み先の `Stats` は使わないので、置き場ごと使い捨てて構わない
    /// (observer が無いので [`BodyObservation::finish_usage`] はディスクに
    /// 触らない)。
    fn plain(body: BodyStream, span: Span) -> BodyObservation {
        let dir = tempfile::tempdir().unwrap();
        observe(
            body,
            None,
            Arc::new(Stats::new(dir.path(), "test")),
            0,
            None,
            "m",
            span,
        )
    }

    /// ログの行から `名前=数値` を取り出す。
    fn field(line: &str, name: &str) -> u128 {
        let rest = line
            .split_once(&format!("{name}="))
            .unwrap_or_else(|| panic!("{name} is missing: {line}"))
            .1;
        rest.split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{name} is not a number: {line}"))
    }

    /// 終端の行を 1 つ取り出す。
    fn line_with<'a>(logs: &'a str, needle: &str) -> &'a str {
        logs.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line containing `{needle}`:\n{logs}"))
    }

    /// 最後まで流れたら、バイト数と一緒に残る。
    #[test]
    fn records_completion_with_byte_count() {
        let logs = logs_of(|| async {
            let mut obs = plain(
                body(vec![
                    Ok(Bytes::from_static(b"event: message_start\n")),
                    Ok(Bytes::from_static(b"data: {}\n")),
                ]),
                request_span(),
            );
            while obs.next().await.is_some() {}
        });

        assert!(logs.contains("finished transferring the body"), "{logs}");
        assert!(
            logs.contains("bytes=30"),
            "the forwarded byte count is known: {logs}"
        );
        assert!(
            logs.contains("elapsed_ms="),
            "the elapsed time is known: {logs}"
        );
    }

    /// 受け取った本文の大きさと、受け取りにかかった時間が残る。
    #[test]
    fn records_the_size_of_the_request_body() {
        let logs = logs_of(|| async {
            let span = request_span();
            record_request_body(&span, 1_300_000, Duration::from_millis(42));
        });

        assert!(logs.contains("received request body"), "{logs}");
        assert!(
            logs.contains("body_bytes=1300000"),
            "how much was received: {logs}"
        );
        assert!(
            logs.contains("elapsed_ms=42"),
            "the time spent receiving is known: {logs}"
        );
    }

    /// upstream のヘッダを受け取った節目が残る。
    #[test]
    fn records_upstream_headers() {
        let logs = logs_of(|| async {
            let span = request_span();
            record_upstream_headers(&span, "claude-opus-5", "anthropic-oauth-a", 200);
        });

        assert!(logs.contains("received upstream headers"), "{logs}");
        assert!(logs.contains("model=claude-opus-5"), "{logs}");
        // 文字列フィールドは引用符付きで出る (tracing の既定の書式)。
        assert!(logs.contains("route=\"anthropic-oauth-a\""), "{logs}");
        assert!(logs.contains("status=200"), "{logs}");
    }

    /// 本文の大きさは、後から出るログにも付いて回る。
    /// 手前のアクセスログと 1 件ずつ突き合わせるための目印なので、
    /// 終端のログだけを見ても分かる必要がある。
    #[test]
    fn the_body_size_stays_on_later_logs() {
        let logs = logs_of(|| async {
            let span = request_span();
            record_request_body(&span, 512, Duration::from_millis(1));
            let mut obs = plain(body(vec![Ok(Bytes::from_static(b"x"))]), span);
            while obs.next().await.is_some() {}
        });

        let end = logs
            .lines()
            .find(|l| l.contains("finished transferring the body"))
            .unwrap_or_else(|| panic!("no record of the end:\n{logs}"));
        assert!(end.contains("body_bytes=512"), "{end}");
    }

    /// 最初のチャンクを送り出した時点が残る。
    /// upstream が返し始めるまでの待ちと、流し切るまでの時間を分けて読む。
    #[test]
    fn records_when_the_first_chunk_goes_out() {
        let logs = logs_of(|| async {
            let mut obs = plain(
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
            .filter(|l| l.contains("sent the first chunk"))
            .collect();
        assert_eq!(first.len(), 1, "only the first one is kept: {logs}");
        assert!(first[0].contains("elapsed_ms="), "{}", first[0]);
    }

    /// 1 バイトも来ないまま終わったら、送り出した記録は残らない。
    /// 「生成待ちのまま終わった」と「流している途中で終わった」は別物。
    #[test]
    fn nothing_is_recorded_when_no_chunk_arrives() {
        let logs = logs_of(|| async {
            let mut obs = plain(
                body(vec![Err(Error::Config(
                    "response reading was interrupted".into(),
                ))]),
                request_span(),
            );
            let _ = obs.next().await;
        });

        assert!(!logs.contains("first chunk"), "{logs}");
        assert!(
            logs.contains("the transfer broke off"),
            "the terminal line remains: {logs}"
        );
    }

    /// チャンクの間で最も長く黙り込んだ時間が残る。
    ///
    /// 平均で見ると詰まりは均されて消える。知りたいのは「途中で何秒
    /// 詰まったか」なので、最長の 1 回を残す。
    #[test]
    fn records_the_longest_silence_between_chunks() {
        let logs = logs_of(|| async {
            let mut obs = plain(
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

        let end = line_with(&logs, "finished transferring the body");
        let gap = field(end, "max_gap_ms");
        assert!(gap >= 60, "picks up the longest silence (80ms gap): {end}");

        // 3 つ目の前で詰まったので、無音の始まりは 1 つ目と 2 つ目の分だけ後。
        let at = field(end, "max_gap_at_ms");
        assert!(
            at < gap,
            "the stall is known to be early on (at={at}, gap={gap})"
        );
    }

    /// 流れている途中で黙り込んだまま切られたら、その無音が残る。
    ///
    /// クライアントが停滞を見て切った場合がこれ。最後のチャンクから
    /// 中断までを数えないと、まさに知りたい区間が抜ける。
    #[test]
    fn silence_before_an_abort_is_counted() {
        let logs = logs_of(|| async {
            let mut obs = plain(
                paused_body(vec![(0, b"data: 1\n"), (0, b"data: 2\n")]),
                request_span(),
            );
            let _ = obs.next().await;
            // 受け取ったきり黙り込んで、捨てる。
            tokio::time::sleep(Duration::from_millis(80)).await;
        });

        let aborted = line_with(&logs, "the transfer was aborted");
        assert!(
            field(aborted, "max_gap_ms") >= 60,
            "the silence right before leaving is recorded: {aborted}"
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
            let mut obs = plain(
                body(vec![Err(Error::Config(
                    "response reading was interrupted".into(),
                ))]),
                request_span(),
            );
            let _ = obs.next().await;
        });

        let broken = line_with(&logs, "the transfer broke off");
        assert_eq!(field(broken, "max_gap_ms"), 0, "{broken}");
        assert_eq!(field(broken, "bytes"), 0, "{broken}");
    }

    /// 途中で upstream が切れたら、切れたと分かる形で残る。
    #[test]
    fn records_failure_midway() {
        let logs = logs_of(|| async {
            let mut obs = plain(
                body(vec![
                    Ok(Bytes::from_static(b"data: {}\n")),
                    Err(Error::Config("response reading was interrupted".into())),
                ]),
                request_span(),
            );
            while let Some(item) = obs.next().await {
                if item.is_err() {
                    break;
                }
            }
        });

        assert!(logs.contains("the transfer broke off"), "{logs}");
        assert!(
            logs.contains("response reading was interrupted"),
            "what happened: {logs}"
        );
        assert!(logs.contains("bytes=9"), "how far it streamed: {logs}");
    }

    /// 終端まで読まれずに捨てられたら、中断として残る。
    /// クライアントが去った場合がこれ。
    #[test]
    fn records_abort_when_dropped_before_the_end() {
        let logs = logs_of(|| async {
            let mut obs = plain(
                body(vec![
                    Ok(Bytes::from_static(b"data: {}\n")),
                    Ok(Bytes::from_static(b"data: {}\n")),
                ]),
                request_span(),
            );
            // 1 つだけ受け取って捨てる。
            let _ = obs.next().await;
        });

        assert!(logs.contains("the transfer was aborted"), "{logs}");
        assert!(logs.contains("bytes=9"), "how far it streamed: {logs}");
    }

    /// 終端まで流したものを捨てても、中断にはしない。
    #[test]
    fn completed_stream_is_not_reported_as_aborted() {
        let logs = logs_of(|| async {
            let mut obs = plain(body(vec![Ok(Bytes::from_static(b"x"))]), request_span());
            while obs.next().await.is_some() {}
        });

        assert!(!logs.contains("aborted"), "{logs}");
    }

    /// リクエストごとに番号が変わる。同時に流れていても取り違えない。
    #[test]
    fn each_request_gets_its_own_number() {
        let logs = logs_of(|| async {
            for _ in 0..2 {
                let span = request_span();
                span.in_scope(|| info!("marker"));
            }
        });

        let numbers: Vec<&str> = logs
            .lines()
            .filter(|l| l.contains("marker"))
            .filter_map(|l| l.split_once("req=").map(|(_, rest)| rest))
            .collect();
        assert_eq!(numbers.len(), 2, "{logs}");
        assert_ne!(numbers[0], numbers[1], "the number is not reused: {logs}");
    }

    // ---------- usage の抽出 ----------
    //
    // 本文の読み方は provider の [`UsageObserver`] が持つので、ここで確かめる
    // のは「渡した役に通し、その結果を 1 度だけ記録する」ことと「流れる
    // バイト列を変えない」こと。方言ごとの読み取りは preset 側の試験にある。

    /// 2026-07-29T12:00:00Z
    const USAGE_NOW: i64 = 1_785_326_400;

    /// 試験用の観測役。中身を解釈せず、通ったバイト数を input として数える。
    struct ByteCounter {
        seen: u64,
    }

    impl UsageObserver for ByteCounter {
        fn observe(&mut self, chunk: &[u8]) {
            self.seen += chunk.len() as u64;
        }

        fn finish(self: Box<Self>) -> Option<TokenUsage> {
            (self.seen > 0).then(|| {
                let mut usage = TokenUsage::default();
                usage.set(TokenKind::input(), self.seen);
                usage
            })
        }
    }

    fn counter() -> Option<Box<dyn UsageObserver>> {
        Some(Box::new(ByteCounter { seen: 0 }))
    }

    fn stream_of(chunks: Vec<Vec<u8>>) -> BodyStream {
        body(chunks.into_iter().map(|c| Ok(Bytes::from(c))).collect())
    }

    fn new_stats() -> Arc<Stats> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(Stats::new(dir.path(), "test"))
    }

    /// 積んだ input トークン数。1 日分・1 行分しか無い前提で読む。
    fn only_entry_input(counts: &ByDate) -> u64 {
        counts.values().next().expect("one day's worth")["a"]["m"]
            .tokens
            .get(&TokenKind::input())
            .unwrap_or(0)
    }

    /// 観測しながら流し切り、下流に出たバイト列を返す。
    async fn drain(
        chunks: Vec<Vec<u8>>,
        observer: Option<Box<dyn UsageObserver>>,
        stats: &Arc<Stats>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut obs = observe(
            stream_of(chunks),
            observer,
            Arc::clone(stats),
            USAGE_NOW,
            Some("a"),
            "m",
            request_span(),
        );
        while let Some(chunk) = obs.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    /// 通ったバイト列を変えずに、観測役へ渡す。
    #[tokio::test]
    async fn the_body_passes_through_untouched_while_being_observed() {
        let stats = new_stats();
        let body = b"hello, upstream".to_vec();
        // 1 バイトずつに割って、境目の入りうる位置を一度に試す。
        let chunks: Vec<Vec<u8>> = body.iter().map(|b| vec![*b]).collect();

        let out = drain(chunks, counter(), &stats).await;

        assert_eq!(out, body, "not a single byte is changed");
        assert_eq!(
            only_entry_input(&stats.in_memory()),
            body.len() as u64,
            "every chunk reaches the sink"
        );
    }

    /// 読む役が無くても、本文はそのまま流れ、usage は記録しない。
    #[tokio::test]
    async fn a_response_without_an_observer_records_no_usage() {
        let stats = new_stats();
        let body = b"{\"usage\":{\"input_tokens\":99}}".to_vec();

        let out = drain(vec![body.clone()], None, &stats).await;

        assert_eq!(out, body);
        assert!(stats.in_memory().is_empty(), "{:?}", stats.in_memory());
    }

    /// 観測役が「読めなかった」と言えば記録しない。
    ///
    /// `count_tokens` のような usage を載せない応答がこれ。本数だけ増えると、
    /// 使っていない日が「使った日」に見える。
    #[tokio::test]
    async fn nothing_is_recorded_when_the_observer_found_no_usage() {
        let stats = new_stats();
        // 1 バイトも流れないので ByteCounter は None を返す。
        let out = drain(vec![], counter(), &stats).await;

        assert!(out.is_empty());
        assert!(stats.in_memory().is_empty(), "{:?}", stats.in_memory());
    }

    /// 途中で捨てられても、そこまでに読めた分は残る。
    ///
    /// クライアントが去った場合がこれ。届いた分の消費は確定しているので、
    /// 記録から落とすと実際に使った分が消える。
    #[tokio::test]
    async fn an_aborted_stream_still_records_what_was_read() {
        let stats = new_stats();
        {
            let mut obs = observe(
                stream_of(vec![b"12345".to_vec(), b"67890".to_vec()]),
                counter(),
                Arc::clone(&stats),
                USAGE_NOW,
                Some("a"),
                "m",
                request_span(),
            );
            // 1 チャンクだけ読んで捨てる。
            let _ = obs.next().await;
        }

        assert_eq!(
            only_entry_input(&stats.in_memory()),
            5,
            "only what was seen up to that point"
        );
    }

    /// 流し切っても記録は 1 度だけ。
    #[tokio::test]
    async fn a_completed_stream_is_recorded_exactly_once() {
        let stats = new_stats();
        let _ = drain(vec![b"1234".to_vec()], counter(), &stats).await;

        let counts = stats.in_memory();
        let day = counts.values().next().expect("one day's worth");
        assert_eq!(day["a"]["m"].requests, 1);
        assert_eq!(only_entry_input(&counts), 4, "not doubled");
    }

    /// upstream が途切れても、そこまでの分は残り、誤りは下流へ伝わる。
    #[tokio::test]
    async fn a_failing_stream_keeps_what_it_read() {
        let stats = new_stats();
        let broken = body(vec![
            Ok(Bytes::from_static(b"123")),
            Err(Error::Config("response reading was interrupted".into())),
        ]);

        let mut saw_error = false;
        {
            let mut obs = observe(
                broken,
                counter(),
                Arc::clone(&stats),
                USAGE_NOW,
                Some("a"),
                "m",
                request_span(),
            );
            while let Some(item) = obs.next().await {
                if item.is_err() {
                    saw_error = true;
                }
            }
        }

        assert!(saw_error, "the error is not swallowed");
        assert_eq!(only_entry_input(&stats.in_memory()), 3);
    }

    /// 記録先は渡された credential とモデル、時刻はその応答が始まった時。
    #[tokio::test]
    async fn the_record_lands_under_the_route_that_answered() {
        let stats = new_stats();
        {
            let mut obs = observe(
                stream_of(vec![b"12".to_vec()]),
                counter(),
                Arc::clone(&stats),
                USAGE_NOW,
                None,
                "claude-opus-5",
                request_span(),
            );
            while obs.next().await.is_some() {}
        }

        let counts = stats.in_memory();
        let day = counts.values().next().expect("one day's worth");
        assert_eq!(
            day[crate::stats::NO_CREDENTIAL]["claude-opus-5"]
                .tokens
                .get(&TokenKind::input())
                .unwrap_or(0),
            2,
            "a route without a credential is recorded under the reserved name"
        );
    }
}
