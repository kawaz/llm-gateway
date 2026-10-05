//! 試験の間に出たログを、その試験のスレッドだけで集める。
//!
//! 受け取り先は `with_default` でスレッドに差し込む。他の試験は並行して
//! 受け取り先なしで同じログの出どころ (callsite) を通るので、tracing が
//! 出どころごとに覚える「受け取る人がいるか」の答えを、誰の目で出すかが
//! 問題になる。

use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Dispatch, Metadata, Subscriber, span};

/// `run` の間に出たログを集める。
///
/// 中で待つものがあるので、同じスレッドで回るランタイムを渡す (時計と
/// I/O も使える)。
pub(crate) fn logs_of<F: Future<Output = ()>>(run: impl FnOnce() -> F) -> String {
    keep_interest_per_thread();
    let buffer = Buffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run());
    });
    buffer.text()
}

/// どこにも差し込まない受け取り先を 1 つ、プロセスが終わるまで登録しておく。
///
/// Design rationale: tracing-core 0.1.36 は登録済みの受け取り先がちょうど
/// 1 つのとき、出どころの答えを登録簿でなく「その出どころを最初に通った
/// スレッドの受け取り先」で出す (`callsite.rs` の `Rebuilder::JustOne` が
/// `dispatcher::get_default` を読む)。受け取り先の無い試験のスレッドが先に
/// 通ると「誰も受け取らない」が覚えられ、ここで差し込んだ受け取り先にも
/// span やログが届かない (tokio-rs/tracing#3611, #2874)。登録簿に常に 2 つ
/// 以上いれば答えは登録簿の全員から出るので、ここで 1 つ足して近道を
/// 使わせない。足す側は最大レベルを上げないよう OFF を名乗る (足した瞬間に
/// まだ答えを出す人がいない出どころを登録させない)。
fn keep_interest_per_thread() {
    static DORMANT: OnceLock<Dispatch> = OnceLock::new();
    DORMANT.get_or_init(|| Dispatch::new(Dormant));
}

/// 何も受け取らず、最大レベルを上げない受け取り先。
struct Dormant;

impl Subscriber for Dormant {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::never()
    }
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::OFF)
    }
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        false
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

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

/// 受け取り先の無いスレッドが先に通った出どころでも、差し込んだ受け取り先には届く。
///
/// 出どころはこの試験だけのものなので、プロセスの中で初めて通るのは
/// 必ず受け取り先の無いスレッドの方になる。
#[test]
fn a_callsite_first_seen_without_a_subscriber_still_reaches_the_capture() {
    fn site() -> tracing::Span {
        tracing::info_span!("first_seen_elsewhere")
    }
    let logs = logs_of(|| async {
        std::thread::spawn(|| drop(site())).join().unwrap();
        site().in_scope(|| tracing::info!("marker"));
    });
    assert!(logs.contains("first_seen_elsewhere"), "{logs}");
}
