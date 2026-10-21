//! 系列ごとの書き手のスレッド。
//!
//! 中継の経路は [`Message`] を上限つきの列へ置くだけで、ディスクには触らない
//! (reference `model` の「同期ブロッキングを置かない」)。ここが列から取り出して
//! 書く。`index.jsonl` の行はバッファに溜め、[`FLUSH`] 以内に追記する
//! (reference `files` の「1 秒以内には必ず追記する」)。
//!
//! 書けなかった時は系列を `error` で止める。中継へは何も返さない。系列が止まったら
//! 列に残っている分を書き切り、`stop` の行を最後に書いて終わる。

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{Map, Value, json};
use tracing::warn;

use super::{Series, StopReason, lock, open_private};
use crate::credential::time::now_unix_ms;

/// 受け渡しの列の長さ。溢れたら書き手が追いつけていないとみなし、系列を止める。
///
/// 1 件はイベント 1 行か本文の 1 チャンク (SSE なら数 KB)。並列の数本が同時に
/// 流れても、ディスクが詰まらない限り溢れない長さ。
const QUEUE: usize = 4096;

/// `index.jsonl` を書き足す間隔の上限。
pub(super) const FLUSH: Duration = Duration::from_millis(500);

/// follow の送信の列の長さ (行数)。購読者が読まずに溢れたら `lagged` で切る
/// (止まったまま繋がっている購読者に、本文をメモリへ抱え続けさせない)。
pub(super) const FOLLOW_QUEUE: usize = 1024;

/// follow の購読者へ流す口。
pub(super) struct Follow {
    pub(super) tx: tokio::sync::mpsc::Sender<String>,
    /// メタ json と本文も同梱する。
    pub(super) embed: bool,
}

/// 書き手へ渡すもの。中継の経路で JSON にはしない。
pub(crate) enum Message {
    /// `index.jsonl` の 1 行 (中継の経路から)。
    Event(super::record::Event),
    /// `*.request.json`。書いた後は変えない。
    RequestMeta(Arc<super::record::RequestMeta>),
    /// `*.<attempt>.response.json`。書いた後は変えない。
    ResponseMeta(Arc<super::record::ResponseMeta>),
    /// 本文 (`*.body`) の続き。最初の 1 回でファイルを作る。書き終えたら
    /// (書けなくても) その大きさを fs の未確定分から外す。
    Body { name: Arc<str>, chunk: Bytes },
    /// 本文のファイルを作る (中身はまだ無い)。
    Open { name: Arc<str> },
    /// 止まったことに気づかせる。
    Wake,
}

/// 書き手への口。
pub(crate) struct Writer {
    tx: SyncSender<Message>,
}

impl Writer {
    pub(super) fn channel() -> (Self, Receiver<Message>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
        (Self { tx }, rx)
    }

    /// 待たずに渡す。溢れていたら、その理由を返す。
    pub(super) fn try_send(&self, message: Message) -> Result<(), &'static str> {
        match self.tx.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("the queue is full"),
            Err(TrySendError::Disconnected(_)) => Err("the writer has gone"),
        }
    }

    pub(super) fn wake(&self) {
        let _ = self.tx.try_send(Message::Wake);
    }
}

/// 書き手のスレッドを起こす。`start` の行を最初に書く。
pub(super) fn spawn(
    series: Arc<Series>,
    rx: Receiver<Message>,
    start: Map<String, Value>,
    follow: Option<Follow>,
    persist: bool,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let index = open_private(&series.dir.join("index.jsonl"), true)?;
    std::thread::Builder::new()
        .name(format!("dump-{}", series.id))
        .spawn(move || {
            let mut writing = Writing {
                series,
                index: std::io::BufWriter::new(index),
                pending: false,
                last_flush: Instant::now(),
                bodies: HashMap::new(),
                follow,
                persist,
            };
            writing.event(start);
            writing.run(rx);
        })
}

struct Writing {
    series: Arc<Series>,
    index: std::io::BufWriter<std::fs::File>,
    /// バッファに書き足していない行があるか。
    pending: bool,
    last_flush: Instant,
    bodies: HashMap<String, std::fs::File>,
    follow: Option<Follow>,
    /// 止まった時にディレクトリを残すか (`--no-persist` なら消す)。
    persist: bool,
}

impl Writing {
    fn run(mut self, rx: Receiver<Message>) {
        loop {
            match rx.recv_timeout(FLUSH) {
                Ok(message) => self.handle(message),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.pending && self.last_flush.elapsed() >= FLUSH {
                self.flush();
            }
            if !self
                .series
                .active
                .load(std::sync::atomic::Ordering::Acquire)
            {
                // 止まる前に列へ置かれた分は書き切る。止まった後は置かれない。
                while let Ok(message) = rx.try_recv() {
                    self.handle(message);
                }
                break;
            }
        }
        self.finish();
    }

    fn handle(&mut self, message: Message) {
        let result = match message {
            Message::Event(event) => {
                self.event(event.to_map());
                Ok(())
            }
            Message::RequestMeta(meta) => self.meta(&meta.file, &meta.to_json()),
            Message::ResponseMeta(meta) => self.meta(&meta.file, &meta.to_json()),
            Message::Open { name } => self.body(&name, None),
            Message::Body { name, chunk } => {
                let written = self.body(&name, Some(&chunk));
                self.series.usage.written(chunk.len() as u64);
                written
            }
            Message::Wake => Ok(()),
        };
        if let Err(e) = result {
            self.failed(&e);
        }
    }

    fn event(&mut self, event: Map<String, Value>) {
        let line = Value::Object(event).to_string();
        let mut written = line.clone();
        written.push('\n');
        if let Err(e) = self.index.write_all(written.as_bytes()) {
            self.failed(&e);
            return;
        }
        self.pending = true;
        self.forward(line);
    }

    fn meta(&mut self, name: &str, json: &Value) -> std::io::Result<()> {
        let mut text = serde_json::to_vec_pretty(json).map_err(std::io::Error::other)?;
        text.push(b'\n');
        open_private(&self.series.dir.join(name), false)?.write_all(&text)?;
        if self.follow.as_ref().is_some_and(|f| f.embed) {
            self.forward(json!({"event": "file", "file": name, "content": json}).to_string());
        }
        Ok(())
    }

    fn body(&mut self, name: &str, chunk: Option<&Bytes>) -> std::io::Result<()> {
        if !self.bodies.contains_key(name) {
            let file = open_private(&self.series.dir.join(name), true)?;
            self.bodies.insert(name.to_owned(), file);
        }
        if let (Some(chunk), Some(file)) = (chunk, self.bodies.get_mut(name)) {
            file.write_all(chunk)?;
            self.series.usage.refresh(now_unix_ms(), false);
            // 同梱する本文は、ファイルに書けた分だけ (書けない本文は流さない)。
            if self.follow.as_ref().is_some_and(|f| f.embed) {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD.encode(chunk);
                self.forward(
                    json!({"event": "body", "file": name, "data_base64": data}).to_string(),
                );
            }
        }
        Ok(())
    }

    /// follow の購読者へ 1 行渡す。待たない: 溢れたら `lagged`、閉じていたら
    /// `disconnected` で系列を止め、以後は流さない。
    fn forward(&mut self, line: String) {
        let Some(follow) = &self.follow else {
            return;
        };
        let reason = match follow.tx.try_send(line) {
            Ok(()) => return,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => StopReason::Lagged,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => StopReason::Disconnected,
        };
        warn!(dump_id = %self.series.id, reason = reason.as_str(), "the follower went away or fell behind; stopping the dump");
        self.follow = None;
        self.series.stop(reason, now_unix_ms());
    }

    fn flush(&mut self) {
        if let Err(e) = self.index.flush() {
            self.failed(&e);
        }
        self.pending = false;
        self.last_flush = Instant::now();
    }

    /// 書けなかった。系列を止めて log に出す。中継には何も返さない。
    fn failed(&self, error: &std::io::Error) {
        warn!(%error, dump_id = %self.series.id, "could not write the dump; stopping it");
        self.series.stop(StopReason::Error, now_unix_ms());
    }

    /// `stop` の行を最後に書いて閉じる。
    fn finish(mut self) {
        let stopped = *lock(&self.series.stopped);
        let (reason, at) = stopped.unwrap_or((StopReason::Error, now_unix_ms()));
        let mut event = Map::new();
        event.insert("event".to_owned(), json!("stop"));
        event.insert("ts".to_owned(), json!(at));
        event.insert("reason".to_owned(), json!(reason.as_str()));
        event.insert("stopped_at".to_owned(), json!(at));
        event.insert("bytes_written".to_owned(), json!(self.series.written()));
        let line = Value::Object(event).to_string();
        let _ = self.index.write_all(format!("{line}\n").as_bytes());
        let _ = self.index.flush();
        self.bodies.clear();
        // follow には `stop` の行を流してから閉じる。溢れていれば行は諦める。
        if let Some(follow) = self.follow.take() {
            let _ = follow.tx.try_send(line);
        }
        if !self.persist
            && let Err(e) = std::fs::remove_dir_all(&self.series.dir)
        {
            warn!(%e, dump_id = %self.series.id, "could not remove a dump that was not to be kept");
        }
    }
}
