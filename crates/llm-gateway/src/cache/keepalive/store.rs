//! 見張りを再起動を跨いで残す置き場 (DR-0024 §2)。
//!
//! keepalive の見張りはプロセス内のメモリにある。動いている会話なら次の
//! リクエストで張り直されるが、**止まっている会話は誰も張り直さない** —
//! そこを繋ぐのが keepalive の仕事なので、リリースのたびに全部落とすと
//! 意味がない。
//!
//! 置き場と書き方は日次集計 ([`crate::stats`]) と同じ流儀にする — 同じ
//! ディレクトリに**待ち受けごとのファイル**を持ち、一時ファイルへ書いてから
//! 差し替える (DR-0011)。同じ置き場を共有する別プロセスの分を上書きしない。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::persist::{sanitize_writer, write_atomically};

/// 残しておく 1 系列。時刻は Unix 秒 (単調時計は保存できない)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub session_id: String,
    pub prefix: String,
    pub ns: String,
    pub model: String,
    pub route: String,
    /// 次に合図を出す予定の時刻。
    pub fires_at: i64,
    /// この系列の cache が消える時刻。過ぎていれば張り直す意味がない。
    pub expires_at: i64,
    /// 合図を出し続ける期間の終わり。
    pub horizon_end: i64,
    /// 自分が出す番か ([`Kind::Primary`])、別のプロセスの後ろに控えているか。
    pub kind: Kind,
}

/// 合図を止めてある会話 1 つ。
///
/// 止めたのは人の意思なので、再起動で解けては困る。解くのは
/// **その会話から実リクエストが来たとき**だけ (DR-0024 §2 追補)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    pub session_id: String,
    /// 止めた時刻。ここから離れすぎた停止は、起動時に捨てる。
    pub paused_at: i64,
}

/// 置き場に書く一式。
///
/// 見張りと停止は同じ待ち受けの持ち物なので、1 ファイルにまとめて 1 回で
/// 差し替える。別ファイルにすると、片方だけ書けた状態が生まれる。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    #[serde(default)]
    pub watched: Vec<Saved>,
    #[serde(default)]
    pub paused: Vec<Paused>,
}

/// 読み込むときだけ、見張りの配列だけが書かれた形も受ける。
///
/// 停止を持たなかった頃に書かれたファイルが手元に残っている。読めなければ
/// 止まっている会話の見張りを 1 世代ぶん落とすことになるので、両方受ける。
#[derive(Deserialize)]
#[serde(untagged)]
enum Read {
    Kept(Kept),
    Watched(Vec<Saved>),
}

impl From<Read> for Kept {
    fn from(read: Read) -> Self {
        match read {
            Read::Kept(kept) => kept,
            Read::Watched(watched) => Kept {
                watched,
                paused: Vec::new(),
            },
        }
    }
}

/// 見張りの立ち位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Primary,
    Standby,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Standby => "standby",
        }
    }
}

/// 待ち受けごとの 1 ファイル。
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// 日次集計と同じ置き場の下に、待ち受けごとのファイルを持つ。
    pub fn new(dir: impl Into<PathBuf>, listen: &str) -> Self {
        Self {
            path: dir
                .into()
                .join("keepalive")
                .join(format!("{}.json", sanitize_writer(listen))),
        }
    }

    /// 前回の見張り。読めなければ空から始める。
    ///
    /// 読めないことは転送を止める理由にならない — 失うのは止まっている会話の
    /// 延長の機会だけで、次の実リクエストで張り直される。
    pub fn load(&self) -> Kept {
        let raw = match std::fs::read(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Kept::default(),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), %e, "cannot read the saved cache keepalive watch");
                return Kept::default();
            }
        };
        match serde_json::from_slice::<Read>(&raw) {
            Ok(read) => read.into(),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), %e, "the saved cache keepalive watch is unreadable; starting empty");
                Kept::default()
            }
        }
    }

    /// 今の見張りと停止を丸ごと書く。
    pub fn save(&self, watched: &Kept) {
        if let Some(dir) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            tracing::warn!(path = %dir.display(), %e, "cannot create the cache keepalive directory");
            return;
        }
        if let Err(e) = write_atomically(&self.path, &watched) {
            tracing::warn!(path = %self.path.display(), %e, "cannot save the cache keepalive watch");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(session: &str) -> Saved {
        Saved {
            session_id: session.to_owned(),
            prefix: "2cf24dba".to_owned(),
            ns: "default".to_owned(),
            model: "m".to_owned(),
            route: "a".to_owned(),
            fires_at: 1_800_003_300,
            expires_at: 1_800_003_570,
            horizon_end: 1_800_028_800,
            kind: Kind::Primary,
        }
    }

    fn kept(watched: Vec<Saved>) -> Kept {
        Kept {
            watched,
            paused: Vec::new(),
        }
    }

    /// 書いたものがそのまま読み戻る。
    #[test]
    fn what_was_written_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path(), "127.0.0.1:11301");

        assert_eq!(
            store.load(),
            Kept::default(),
            "nothing has been written yet"
        );

        let written = Kept {
            watched: vec![saved("s-1"), saved("s-2")],
            paused: vec![Paused {
                session_id: "s-3".to_owned(),
                paused_at: 1_800_000_000,
            }],
        };
        store.save(&written);
        assert_eq!(store.load(), written);
    }

    /// 停止を持たなかった頃のファイル (見張りの配列だけ) も読める。
    #[test]
    fn a_file_written_without_the_pauses_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path(), "127.0.0.1:11301");
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, serde_json::to_vec(&[saved("s-1")]).unwrap()).unwrap();

        assert_eq!(store.load(), kept(vec![saved("s-1")]));
    }

    /// 待ち受けごとに別のファイルを持つ。同じ置き場を共有しても上書きしない。
    #[test]
    fn each_listener_keeps_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let one = Store::new(dir.path(), "127.0.0.1:11301");
        let other = Store::new(dir.path(), "127.0.0.1:11302");

        one.save(&kept(vec![saved("s-1")]));
        other.save(&kept(vec![saved("s-2")]));

        assert_eq!(one.load().watched[0].session_id, "s-1");
        assert_eq!(other.load().watched[0].session_id, "s-2");
    }

    /// 壊れたファイルは、空から始める理由にしかならない。
    #[test]
    fn an_unreadable_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path(), "127.0.0.1:11301");
        store.save(&kept(vec![saved("s-1")]));
        std::fs::write(&store.path, b"{ this is not json").unwrap();

        assert_eq!(store.load(), Kept::default());
    }
}
