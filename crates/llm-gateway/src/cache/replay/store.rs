//! 系列ごとに「最後に転送した 1 本」を置く (DR-0027 決定 3)。
//!
//! **ここに載るのは会話の本文そのもの**である (DR-0027 決定 4)。同意フラグも
//! 暗号化もマスキングも持たない — 同じホストにはセッションの transcript が
//! 丸ごと置いてあり、tap (DR-0017) からも本文は読めるので、ここだけに保護の
//! 儀式を足しても守られるものは増えない。置き場を配る・共有するときは、
//! 会話の中身がそのまま入っている前提で扱う。
//!
//! 置き場は待ち受けごとに分けない。11301 と 11302 は**同じファイルを共有**し、
//! 脇の `.lock` を掴んだ 1 台だけが撫でる (DR-0010 と同型)。合図方式では
//! 共有状態を持たない前提だったので観測だけで 1 本へ収束させる規則が要ったが、
//! 自送信は gateway が自分で出す 1 本なので、ファイルの排他がそのまま重複
//! 防止になる。
//!
//! 書き方は他の置き場と同じ流儀 — 一時ファイルへ書いてから rename する
//! ([`write_atomically`], DR-0011)。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::egress::RequestShape;
use crate::persist::write_atomically;

/// 1 系列に持たせてよい本文の大きさ (バイト)。
///
/// 30K トークンのプレフィックスで 200KB 前後、道具と system を厚く積んだ
/// 会話でも 1MB を大きくは超えない。8MB はその 1 桁上に置いた蓋で、ここを
/// 超える系列は**保持しない** = 延命しない。置き場全体に上限を掛けないのは、
/// 系列の数は会話の数で頭打ちになり、1 本ずつの蓋があれば総量も抑えられる
/// ため (DR-0027「未確定」への回答)。
pub const BODY_LIMIT: usize = 8 * 1024 * 1024;

/// 系列 1 つ分の控え。時刻は全て Unix ミリ秒 (単調時計は保存できない)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub session_id: String,
    pub prefix: String,
    pub ns: String,
    /// 解決後の実モデル名 (クライアントが名乗った短い名前ではない)。
    pub model: String,
    /// 直前の実リクエストが通った経路の名前。
    pub route: String,
    /// 送出直前の本文。認証の差し替えとモデル名の書き換えを済ませた形。
    pub body: Value,
    /// 送出直前のヘッダ。**認証は入っていない** — 認証は経路が送るときに
    /// 付け直すので、控えに持つと古い token を持ち回ることになる。
    pub headers: Vec<(String, String)>,
    pub path: String,
    pub query: Option<String>,
    pub shape: RequestShape,
    /// 次に送り直す予定の時刻。
    pub fires_at_ms: i64,
    /// この系列の cache が消える時刻。過ぎていれば送っても繋ぐものが無い。
    pub expires_at_ms: i64,
    /// 送り直し続ける期間の終わり。延ばせるのは実リクエストだけ。
    pub horizon_end_ms: i64,
    /// この連鎖の起点 = 最後に来た実リクエストを送った時刻。
    pub since_ms: i64,
    /// ここまでに送り直した本数。
    #[serde(default)]
    pub count: u32,
    /// この系列について最後に約束した寿命の id (`cache_notice`、DR-0012)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_notice: Option<String>,
}

/// 系列を指す鍵。ファイル名にもなる。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Series {
    pub session_id: String,
    pub prefix: String,
}

impl Series {
    /// ファイル名 (拡張子なし)。
    ///
    /// 会話の id は client が名乗るものなので、区切りやパスに使える文字が
    /// 混ざりうる。英数字以外を潰したうえで、潰した結果が衝突しないよう
    /// **prefix を後ろに付ける** — prefix はこちらが本文から作る 16 進なので
    /// そのまま名前に使える。
    fn stem(&self) -> String {
        let cleaned: String = self
            .session_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let trimmed = cleaned.trim_matches('-');
        let session = if trimmed.is_empty() {
            "unknown"
        } else {
            trimmed
        };
        format!("{session}.{}", self.prefix)
    }
}

/// 系列の控えを置くディレクトリ 1 つ。
pub struct Store {
    dir: PathBuf,
}

/// 掴んでいる間だけ、この系列を撫でてよい ([`Store::claim`])。
///
/// 落とすと flock が外れ、待っている兄弟が掴めるようになる。
pub struct Claim {
    _file: std::fs::File,
}

impl Store {
    /// 日次集計と同じ置き場の下、`keepalive/` に系列ごとのファイルを持つ。
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().join("keepalive"),
        }
    }

    fn path_of(&self, series: &Series) -> PathBuf {
        self.dir.join(format!("{}.json", series.stem()))
    }

    fn lock_path_of(&self, series: &Series) -> PathBuf {
        self.dir.join(format!("{}.lock", series.stem()))
    }

    /// この系列を撫でる権利を掴む。**兄弟が掴んでいれば `None`**。
    ///
    /// 待たないのは、待って掴めた頃には相手が送り終えているため — その系列は
    /// 既に延びていて、こちらが続けて送る理由が無い。
    ///
    /// `.lock` は消さない。消して作り直すと、掴んでいる側と後から来た側が別の
    /// ファイルを見ることになり、締め出しが破れる (DR-0010 と同じ理由)。
    pub fn claim(&self, series: &Series) -> Option<Claim> {
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(path = %self.dir.display(), %e, "cannot create the cache replay directory");
            return None;
        }
        let path = self.lock_path_of(series);
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "cannot open the cache replay lock");
                return None;
            }
        };
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Some(Claim { _file: file }),
            Err(rustix::io::Errno::WOULDBLOCK) => None,
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "cannot take the cache replay lock");
                None
            }
        }
    }

    /// この系列の控え。無ければ `None`。
    pub fn load(&self, series: &Series) -> Option<Kept> {
        let path = self.path_of(series);
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "cannot read the kept request");
                return None;
            }
        };
        match serde_json::from_slice(&raw) {
            Ok(kept) => Some(kept),
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "the kept request is unreadable; dropping it");
                None
            }
        }
    }

    /// 置いてある控えを全部読む。起動時に予定を張り直すために使う。
    pub fn load_all(&self) -> Vec<Kept> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut all = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            match std::fs::read(&path).map(|raw| serde_json::from_slice::<Kept>(&raw)) {
                Ok(Ok(kept)) => all.push(kept),
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), %e, "the kept request is unreadable; dropping it");
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "cannot read the kept request");
                }
            }
        }
        all
    }

    /// この系列の控えを書く。大きすぎる本文は**持たない** (= 延命しない)。
    ///
    /// 書けたかどうかを返す。書けなかったことは転送を止める理由にならない —
    /// 失うのは止まった会話を繋ぐ機会だけ。
    pub fn save(&self, kept: &Kept) -> bool {
        let series = Series {
            session_id: kept.session_id.clone(),
            prefix: kept.prefix.clone(),
        };
        let size = match serde_json::to_vec(&kept.body) {
            Ok(json) => json.len(),
            Err(e) => {
                tracing::warn!(%e, "cannot measure the request to keep");
                return false;
            }
        };
        if size > BODY_LIMIT {
            tracing::info!(
                session = %kept.session_id,
                prefix = %kept.prefix,
                bytes = size,
                limit = BODY_LIMIT,
                "this conversation is too large to keep; it will not be replayed"
            );
            self.remove(&series);
            return false;
        }
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(path = %self.dir.display(), %e, "cannot create the cache replay directory");
            return false;
        }
        let path = self.path_of(&series);
        if let Err(e) = write_atomically(&path, kept) {
            tracing::warn!(path = %path.display(), %e, "cannot keep the request for replay");
            return false;
        }
        true
    }

    /// この系列の控えを捨てる。`.lock` は残す (掴んでいる相手が居る)。
    pub fn remove(&self, series: &Series) {
        let path = self.path_of(series);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), %e, "cannot drop the kept request");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(session: &str) -> Series {
        Series {
            session_id: session.to_owned(),
            prefix: "2cf24dba".to_owned(),
        }
    }

    fn kept(session: &str, body: Value) -> Kept {
        Kept {
            session_id: session.to_owned(),
            prefix: "2cf24dba".to_owned(),
            ns: "default".to_owned(),
            model: "claude-opus-5".to_owned(),
            route: "a".to_owned(),
            body,
            headers: vec![("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned())],
            path: "/v1/messages".to_owned(),
            query: None,
            shape: RequestShape::Messages,
            fires_at_ms: 1_800_003_300_000,
            expires_at_ms: 1_800_003_570_000,
            horizon_end_ms: 1_800_028_800_000,
            since_ms: 1_800_000_000_000,
            count: 2,
            cache_notice: Some("n-1".to_owned()),
        }
    }

    /// 書いたものがそのまま読み戻る。
    #[test]
    fn what_was_written_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        assert_eq!(store.load(&series("s-1")), None, "nothing is kept yet");

        let written = kept("s-1", serde_json::json!({"messages": [{"role": "user"}]}));
        assert!(store.save(&written));
        assert_eq!(store.load(&series("s-1")), Some(written));
    }

    /// 系列ごとに別のファイル。同じ会話でも prefix が違えば別の控え。
    #[test]
    fn each_series_keeps_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        store.save(&kept("s-1", serde_json::json!({"a": 1})));
        let mut other = kept("s-1", serde_json::json!({"a": 2}));
        other.prefix = "ffffffff".to_owned();
        store.save(&other);

        assert_eq!(store.load(&series("s-1")).unwrap().body["a"], 1);
        assert_eq!(
            store
                .load(&Series {
                    session_id: "s-1".to_owned(),
                    prefix: "ffffffff".to_owned(),
                })
                .unwrap()
                .body["a"],
            2
        );
    }

    /// 上限を超えた本文は保持しない = その系列は延命しない。
    #[test]
    fn a_conversation_too_large_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        let huge = serde_json::json!({ "text": "x".repeat(BODY_LIMIT + 1) });
        assert!(!store.save(&kept("s-1", huge)));
        assert_eq!(store.load(&series("s-1")), None);
    }

    /// 大きくなった系列は、それまでの控えごと落ちる。
    ///
    /// 残しておくと、伸びた会話のプレフィックスからとうに外れた古い本文を
    /// 撫で続けることになる。
    #[test]
    fn growing_past_the_limit_drops_what_was_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        assert!(store.save(&kept("s-1", serde_json::json!({"a": 1}))));
        let huge = serde_json::json!({ "text": "x".repeat(BODY_LIMIT + 1) });
        assert!(!store.save(&kept("s-1", huge)));

        assert_eq!(store.load(&series("s-1")), None);
    }

    /// 撫でてよいのは 1 台だけ。掴んでいる間、後から来た側は掴めない。
    #[test]
    fn only_one_may_touch_a_series() {
        let dir = tempfile::tempdir().unwrap();
        let one = Store::new(dir.path());
        let other = Store::new(dir.path());

        let claim = one.claim(&series("s-1")).expect("the first claim wins");
        assert!(
            other.claim(&series("s-1")).is_none(),
            "the sibling must not touch the same series"
        );
        assert!(
            other.claim(&series("s-2")).is_some(),
            "another series is free"
        );

        drop(claim);
        assert!(
            other.claim(&series("s-1")).is_some(),
            "the lock is released with the claim"
        );
    }

    /// 置いてある控えを全部読み戻せる。壊れたファイルは飛ばす。
    #[test]
    fn everything_kept_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&kept("s-1", serde_json::json!({"a": 1})));
        store.save(&kept("s-2", serde_json::json!({"a": 2})));
        std::fs::write(store.dir.join("broken.json"), b"{ not json").unwrap();

        let mut sessions: Vec<String> = store
            .load_all()
            .into_iter()
            .map(|kept| kept.session_id)
            .collect();
        sessions.sort();
        assert_eq!(sessions, ["s-1", "s-2"]);
    }

    /// 捨てた控えは読めなくなる。
    #[test]
    fn what_was_dropped_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&kept("s-1", serde_json::json!({"a": 1})));

        store.remove(&series("s-1"));
        assert_eq!(store.load(&series("s-1")), None);
        store.remove(&series("s-1"));
    }
}
