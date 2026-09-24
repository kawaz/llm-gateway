//! 起きたことを見ている人へ流す口 (DR-0012)。
//!
//! 流すのは起きたことだけで、状態は持たない。誰も見ていなければ何もしない。
//! 見ている人が遅れたら、その人の分は落ちる。
//!
//! 知らせの中身の型 `E` は利用側が決める。ここが持つのは、通し番号と起動の印を
//! 押して配ること、落とした数を数えることだけ。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::broadcast;

/// 溜めておける数。
///
/// 見ている人が遅れた分はここを溢れて落ちる。大きくしても古い知らせが
/// まとめて届くだけで、今起きていることを追う相手の役には立たない。
pub const BACKLOG: usize = 256;

/// 通し番号と起動の印を押せる知らせ。
pub trait Stamped: Clone + Send + 'static {
    /// 押すのは流す 1 箇所 ([`Events::publish`]) だけ。
    fn stamp(&mut self, seq: u64, boot: i64);
}

/// 見ている人へ配る口。
pub struct Events<E> {
    tx: broadcast::Sender<E>,
    /// 最後に振った通し番号。
    ///
    /// Design rationale: 原子的な加算ではなく錠で持つ。番号を振ってから流す
    /// までの間に別の送り手が割り込むと、番号の順と届く順が入れ替わり、見る側が
    /// 「欠けた」と誤読する。錠の中で振って流せば、届く順 = 番号の順になる。
    /// 流すのは溜め置きへの書き込みだけで待たないので、錠を持つ時間は短い。
    seq: Mutex<u64>,
    /// この起動の印 (口を作った時刻、Unix ミリ秒)。
    boot: i64,
    /// 見ている人が追いつけずに落とした数 (起動からの累積、全員の合計)。
    dropped: Arc<AtomicU64>,
}

impl<E: Stamped> Default for Events<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Stamped> Events<E> {
    pub fn new() -> Self {
        Self {
            tx: broadcast::Sender::new(BACKLOG),
            seq: Mutex::new(0),
            boot: crate::credential::time::now_unix_ms(),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 1 件流す。
    ///
    /// 誰も見ていなければ何もしない。**流す側の邪魔をしないこと**が第一で、
    /// 配れなかったことを流した側へ持ち帰らない (待たない・失敗にしない)。
    ///
    /// 番号はここで振る。誰も見ていなくても進める — 見る側が比べるのは
    /// 自分が受け取った番号同士なので、繋ぐ前の分は比較に現れない。
    pub fn publish(&self, notice: impl Into<E>) {
        let mut notice = notice.into();
        let mut seq = self.seq.lock().unwrap_or_else(PoisonError::into_inner);
        *seq += 1;
        notice.stamp(*seq, self.boot);
        let _ = self.tx.send(notice);
    }

    /// この起動の印。再起動で通し番号が 1 に戻ったことを、見る側がこれの
    /// 変化で知る。
    pub fn boot(&self) -> i64 {
        self.boot
    }

    /// 見る側に回る。届くのは**これ以降**の分だけ。
    pub fn subscribe(&self) -> Watching<E> {
        Watching {
            rx: self.tx.subscribe(),
            dropped: Arc::clone(&self.dropped),
        }
    }

    /// 見ている人が追いつけずに落とした数。起動からの累積で、全員の合計。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 今この口を見ている人の数。
    pub fn watchers(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// 1 人ぶんの見る口。
///
/// 落とした数は購読の入口で数える。見る側ごとに数えさせると、新しく足した
/// 見る側が数え忘れても気づけない。
pub struct Watching<E> {
    rx: broadcast::Receiver<E>,
    dropped: Arc<AtomicU64>,
}

impl<E: Clone> Watching<E> {
    /// 次の 1 件。追いつけなかったときは、落とした数を足してから知らせる。
    pub async fn recv(&mut self) -> Result<E, broadcast::error::RecvError> {
        let received = self.rx.recv().await;
        if let Err(broadcast::error::RecvError::Lagged(missed)) = &received {
            self.dropped.fetch_add(*missed, Ordering::Relaxed);
        }
        received
    }

    /// 待たずに次の 1 件。落とした数の数え方は [`Self::recv`] と同じ。
    pub fn try_recv(&mut self) -> Result<E, broadcast::error::TryRecvError> {
        let received = self.rx.try_recv();
        if let Err(broadcast::error::TryRecvError::Lagged(missed)) = &received {
            self.dropped.fetch_add(*missed, Ordering::Relaxed);
        }
        received
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Tick {
        seq: u64,
        boot: i64,
    }

    impl Stamped for Tick {
        fn stamp(&mut self, seq: u64, boot: i64) {
            self.seq = seq;
            self.boot = boot;
        }
    }

    fn tick() -> Tick {
        Tick { seq: 0, boot: 0 }
    }

    #[test]
    fn numbers_follow_the_publishing_order() {
        let events = Events::<Tick>::new();
        let mut watching = events.subscribe();
        events.publish(tick());
        events.publish(tick());
        let boot = events.boot();
        assert_eq!(watching.try_recv().unwrap(), Tick { seq: 1, boot });
        assert_eq!(watching.try_recv().unwrap(), Tick { seq: 2, boot });
    }

    #[test]
    fn a_lagging_watcher_adds_to_dropped() {
        let events = Events::<Tick>::default();
        let mut watching = events.subscribe();
        assert_eq!(events.watchers(), 1);
        for _ in 0..BACKLOG + 3 {
            events.publish(tick());
        }
        assert!(matches!(
            watching.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(3))
        ));
        assert_eq!(events.dropped(), 3);
    }
}
