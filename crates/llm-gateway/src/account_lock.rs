//! session の開始 account と「跨いだ」印の置き場 (DR-0034)。
//!
//! thinking が account に束縛されるモデルの session は開始 account に結ばれる
//! (DR-0033)。その結びつきを失うと、跨いだ履歴の thinking が黙って落ちる。
//! restart と兄弟の unit を跨いで覚えておくため、`[stats] dir` の下の
//! `account-lock/locks.json` に 1 つの map として持ち、両 unit で共有する。
//!
//! - 読みはメモリの控えを引く。引く前にファイルの版 (mtime のナノ秒) を見て、
//!   動いていれば読み直す
//! - 書きは状態が変わった時だけ ([`AccountLocks::remember`])。脇の `.lock` を
//!   flock で掴み、最新を読み直して当ててから丸ごと差し替える
//! - 読み書きの失敗は警告を残して進む。ロックは推論の連続性を守る仕組みで、
//!   置き場の障害で転送を止めるほど重くない

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::persist::write_atomically;
use crate::router::AccountLock;

/// 最後に通ってから、ロックを覚えておく時間 (秒)。
pub const LOCK_TTL_SECS: i64 = 24 * 3600;

/// `seen` だけが進んだ時に、ファイルへ書き直す間隔 (秒)。
///
/// `seen` の更新を 2xx ごとに書くと、書き込みがリクエスト数に比例する。
/// 寿命 24 時間に対して 5 分の誤差なら困らない。
const SEEN_STRIDE_SECS: i64 = 300;

/// ロックの鍵。affinity と同じ `(namespace 名, session key, モデル)`。
pub type Key = (String, String, String);

/// ファイルの 1 レコード。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    ns: String,
    session: String,
    model: String,
    account: String,
    crossed: bool,
    /// 最後にこの session が 2xx で通った時刻 (Unix 秒)。
    seen: i64,
}

impl Record {
    fn key(&self) -> Key {
        (self.ns.clone(), self.session.clone(), self.model.clone())
    }

    fn alive(&self, now: i64) -> bool {
        now - self.seen < LOCK_TTL_SECS
    }

    /// 同じ鍵の別の言い分を当てる。`account` は先にあった方 (= self) が勝ち、
    /// `crossed` は OR、`seen` は大きい方。
    ///
    /// 相手の開始 account がこちらと違えば、相手はこちらの開始 account 以外へ
    /// 送ったことになるので、それも跨ぎとして数える。
    fn absorb(&mut self, other: &Record) {
        self.crossed |= other.crossed || other.account != self.account;
        self.seen = self.seen.max(other.seen);
    }
}

/// ファイルの形。鍵の文字列は `[ns, session, model]` の JSON で、区切り文字の
/// 衝突が起きない。
type Map = HashMap<String, Record>;

fn map_key(key: &Key) -> String {
    serde_json::to_string(&[&key.0, &key.1, &key.2]).unwrap_or_default()
}

/// メモリの控え 1 件。
struct Entry {
    record: Record,
    /// ファイルに載っていると分かっている `seen`。まだ載っていなければ `None`。
    written: Option<i64>,
}

#[derive(Default)]
struct Memo {
    /// 控えが写しているファイルの版。
    version: Option<u64>,
    entries: HashMap<Key, Entry>,
}

impl Memo {
    /// ファイルの中身を控えに当てる。account はファイルが正 (別 unit が先に
    /// 入れていればそちら)、印と `seen` は合わせる。
    fn absorb_file(&mut self, map: Map, now: i64) {
        for (_, mut from_file) in map {
            if !from_file.alive(now) {
                continue;
            }
            let key = from_file.key();
            let mine = self.entries.get(&key);
            let written = mine
                .and_then(|e| e.written)
                .map_or(from_file.seen, |w| w.max(from_file.seen));
            if let Some(mine) = mine
                && mine.record.alive(now)
            {
                from_file.absorb(&mine.record);
            }
            let written = Some(written);
            self.entries.insert(
                key,
                Entry {
                    record: from_file,
                    written,
                },
            );
        }
        self.entries.retain(|_, e| e.record.alive(now));
    }
}

/// ロックの表。メモリの控えと、兄弟の unit と共有するファイル。
pub struct AccountLocks {
    path: PathBuf,
    lock_path: PathBuf,
    memo: std::sync::Mutex<Memo>,
    /// 裏で走っている書き込み。試験で書き終わりを待つためだけに持つ。
    writes: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl AccountLocks {
    /// `[stats] dir` の下、`account-lock/` に置く。今の中身を読んで控えに載せる
    /// (刈り込みはしない、寿命切れは読む側で無視する)。
    pub fn open(stats_dir: impl AsRef<Path>) -> Self {
        let dir = stats_dir.as_ref().join("account-lock");
        let locks = Self {
            path: dir.join("locks.json"),
            lock_path: dir.join("locks.json.lock"),
            memo: std::sync::Mutex::new(Memo::default()),
            writes: std::sync::Mutex::new(Vec::new()),
        };
        locks.refresh(&mut locks.memo(), crate::credential::time::now_unix());
        locks
    }

    fn memo(&self) -> std::sync::MutexGuard<'_, Memo> {
        self.memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// この session のロック。無い、または寿命切れなら `None`。
    pub fn get(&self, key: &Key, now: i64) -> Option<AccountLock> {
        let mut memo = self.memo();
        self.refresh(&mut memo, now);
        let entry = memo.entries.get(key)?;
        entry.record.alive(now).then(|| AccountLock {
            account: entry.record.account.clone(),
            crossed: entry.record.crossed,
        })
    }

    /// `account` で 2xx が返ったことを覚える。控えはその場で更新し、状態が
    /// 変わっていればファイルへの書き込みを裏で出す (送信を待たせない)。
    pub fn remember(self: &Arc<Self>, key: Key, account: &str, now: i64) {
        let Some(change) = self.note(key, account, now) else {
            return;
        };
        let this = Arc::clone(self);
        let handle = tokio::task::spawn_blocking(move || this.write(&change, now));
        let mut writes = self
            .writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writes.retain(|h| !h.is_finished());
        writes.push(handle);
    }

    /// 控えを更新し、ファイルへ書くべき変化なら書く中身を返す。
    ///
    /// 書くのは、開始 account が決まった時 / 印が立った時 / `seen` が前に
    /// 書いた値から [`SEEN_STRIDE_SECS`] 以上進んだ時だけ (DR-0034 決定 4)。
    fn note(&self, key: Key, account: &str, now: i64) -> Option<Record> {
        let mut memo = self.memo();
        self.refresh(&mut memo, now);
        match memo.entries.get_mut(&key) {
            Some(entry) if entry.record.alive(now) => {
                let was_crossed = entry.record.crossed;
                entry.record.crossed |= account != entry.record.account;
                entry.record.seen = entry.record.seen.max(now);
                let stale = entry
                    .written
                    .is_none_or(|written| now - written >= SEEN_STRIDE_SECS);
                (entry.record.crossed != was_crossed || stale).then(|| entry.record.clone())
            }
            _ => {
                let record = Record {
                    ns: key.0.clone(),
                    session: key.1.clone(),
                    model: key.2.clone(),
                    account: account.to_owned(),
                    crossed: false,
                    seen: now,
                };
                memo.entries.insert(
                    key,
                    Entry {
                        record: record.clone(),
                        written: None,
                    },
                );
                Some(record)
            }
        }
    }

    /// 版が動いていればファイルを読み直して控えに当てる。
    fn refresh(&self, memo: &mut Memo, now: i64) {
        let version = self.version();
        if version.is_none() || version == memo.version {
            return;
        }
        if let Some(map) = self.read() {
            memo.absorb_file(map, now);
            memo.version = version;
        }
    }

    /// 更新時刻を版として使う。書き換えは rename なので、中身が入れ替われば
    /// 必ず動く (DR-0010)。
    fn version(&self) -> Option<u64> {
        let modified = std::fs::metadata(&self.path).ok()?.modified().ok()?;
        let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(since_epoch.as_nanos() as u64)
    }

    /// ファイルの中身。無ければ空、読めなければ `None` (控えのまま進む)。
    fn read(&self) -> Option<Map> {
        let raw = match std::fs::read(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Map::new()),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), %e, "cannot read the session account locks");
                return None;
            }
        };
        match serde_json::from_slice(&raw) {
            Ok(map) => Some(map),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), %e, "the session account locks are unreadable");
                None
            }
        }
    }

    /// 脇の `.lock` を掴み、最新を読み直して当て、寿命切れを刈って書く。
    ///
    /// 鍵が無い (または寿命切れ) なら自分の account を開始 account として
    /// 入れる。あれば account は変えず、印は OR、`seen` は大きい方
    /// (DR-0034 決定 2)。掴んでいる区間は読み直しから rename までだけ。
    fn write(&self, change: &Record, now: i64) {
        if let Err(e) = self.write_locked(change, now) {
            tracing::warn!(path = %self.path.display(), %e, "cannot save the session account lock");
        }
    }

    fn write_locked(&self, change: &Record, now: i64) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _held = self.hold()?;
        // 読めない中身の上には書かない。書くと相手の開始 account を消しうる。
        let mut map = self
            .read()
            .ok_or_else(|| std::io::Error::other("the current locks are unreadable"))?;
        map.retain(|_, record| record.alive(now));
        let slot = map_key(&change.key());
        match map.get_mut(&slot) {
            Some(existing) => existing.absorb(change),
            None => {
                map.insert(slot, change.clone());
            }
        }
        write_atomically(&self.path, &map)?;
        let version = self.version();
        let mut memo = self.memo();
        memo.absorb_file(map, now);
        memo.version = version;
        Ok(())
    }

    /// 脇の `.lock` を掴む。掴めるまで待つ。`.lock` は消さない (消して作り直すと、
    /// 掴んでいる側と後から来た側が別のファイルを見て締め出しが破れる)。
    fn hold(&self) -> std::io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock_path)?;
        loop {
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
                Ok(()) => return Ok(file),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// 裏で出した書き込みが全部終わるまで待つ。
    #[cfg(test)]
    pub async fn settled(&self) {
        let writes: Vec<_> = std::mem::take(
            &mut *self
                .writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for handle in writes {
            handle.await.unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_800_000_000;

    fn key(s: &str) -> Key {
        ("default".into(), s.into(), "m".into())
    }

    fn lock(account: &str, crossed: bool) -> Option<AccountLock> {
        Some(AccountLock {
            account: account.to_owned(),
            crossed,
        })
    }

    /// 書き込みを同期で当てる (裏の仕事を介さない)。
    fn pass(locks: &AccountLocks, s: &str, account: &str, now: i64) -> bool {
        match locks.note(key(s), account, now) {
            Some(change) => {
                locks.write(&change, now);
                true
            }
            None => false,
        }
    }

    fn on_disk(dir: &Path) -> Map {
        let raw = std::fs::read(dir.join("account-lock/locks.json")).unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    /// 2 つの unit が同じ置き場を見て、先に入れた方の account が残り、後の方の
    /// 跨ぎが OR で入る。
    #[test]
    fn two_units_share_the_first_account_and_the_crossing() {
        let dir = tempfile::tempdir().unwrap();
        let stable = AccountLocks::open(dir.path());
        let unstable = AccountLocks::open(dir.path());

        assert!(pass(&stable, "s", "a", T0));
        // unstable はまだ控えに何も無いまま、別 account で通った。
        assert!(pass(&unstable, "s", "b", T0 + 1));

        assert_eq!(stable.get(&key("s"), T0 + 2), lock("a", true));
        assert_eq!(unstable.get(&key("s"), T0 + 2), lock("a", true));
        let record = &on_disk(dir.path())[&map_key(&key("s"))];
        assert_eq!(
            (record.account.as_str(), record.crossed, record.seen),
            ("a", true, T0 + 1)
        );
    }

    /// 並行した初回: 控えは互いに知らず、flock の内側で先に書いた方が勝つ。
    #[test]
    fn the_first_writer_wins_between_units() {
        let dir = tempfile::tempdir().unwrap();
        let stable = AccountLocks::open(dir.path());
        let unstable = AccountLocks::open(dir.path());
        let first = stable.note(key("s"), "a", T0).unwrap();
        let second = unstable.note(key("s"), "b", T0).unwrap();

        unstable.write(&second, T0);
        stable.write(&first, T0);

        assert_eq!(stable.get(&key("s"), T0), lock("b", true));
        assert_eq!(unstable.get(&key("s"), T0), lock("b", true));
    }

    /// 寿命切れは読み手に見えず、次の書き込みで消える。
    #[test]
    fn expired_locks_are_invisible_and_pruned_on_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let locks = AccountLocks::open(dir.path());
        pass(&locks, "old", "a", T0);
        let later = T0 + LOCK_TTL_SECS;

        let fresh = AccountLocks::open(dir.path());
        assert_eq!(fresh.get(&key("old"), later), None);
        assert_eq!(locks.get(&key("old"), later), None);
        assert!(
            on_disk(dir.path()).contains_key(&map_key(&key("old"))),
            "reading does not prune"
        );

        pass(&fresh, "new", "b", later);
        let disk = on_disk(dir.path());
        assert!(!disk.contains_key(&map_key(&key("old"))));
        assert!(disk.contains_key(&map_key(&key("new"))));
    }

    /// 寿命切れの後に通ったら、その account で開始し直す。
    #[test]
    fn an_expired_lock_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let locks = AccountLocks::open(dir.path());
        pass(&locks, "s", "a", T0);
        pass(&locks, "s", "b", T0 + LOCK_TTL_SECS);
        assert_eq!(locks.get(&key("s"), T0 + LOCK_TTL_SECS), lock("b", false));
    }

    /// `seen` だけが進んだ時は 5 分ごとにしか書かない。印が立てばすぐ書く。
    #[test]
    fn only_state_changes_and_a_stale_seen_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let locks = AccountLocks::open(dir.path());
        assert!(
            pass(&locks, "s", "a", T0),
            "the starting account is written"
        );
        assert!(!pass(&locks, "s", "a", T0 + SEEN_STRIDE_SECS - 1));
        assert_eq!(on_disk(dir.path())[&map_key(&key("s"))].seen, T0);

        assert!(pass(&locks, "s", "a", T0 + SEEN_STRIDE_SECS));
        assert!(
            pass(&locks, "s", "b", T0 + SEEN_STRIDE_SECS + 1),
            "crossing is written"
        );
        assert!(!pass(&locks, "s", "b", T0 + SEEN_STRIDE_SECS + 2));
    }

    /// 書けなかった開始 account は、次の 2xx で書き直す。
    #[test]
    fn a_failed_write_is_retried_on_the_next_success() {
        let dir = tempfile::tempdir().unwrap();
        // 置き場になるはずの場所をファイルで塞ぐ。
        std::fs::write(dir.path().join("account-lock"), b"").unwrap();
        let locks = AccountLocks::open(dir.path());
        assert!(pass(&locks, "s", "a", T0));
        assert_eq!(
            locks.get(&key("s"), T0),
            lock("a", false),
            "the memo still holds it"
        );

        std::fs::remove_file(dir.path().join("account-lock")).unwrap();
        assert!(pass(&locks, "s", "a", T0 + 1));
        assert_eq!(on_disk(dir.path())[&map_key(&key("s"))].account, "a");
    }

    /// 読めないファイルがあっても転送は止めず、上書きもしない。
    #[test]
    fn an_unreadable_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("account-lock")).unwrap();
        std::fs::write(dir.path().join("account-lock/locks.json"), b"{broken").unwrap();
        let locks = AccountLocks::open(dir.path());
        pass(&locks, "s", "a", T0);
        assert_eq!(locks.get(&key("s"), T0), lock("a", false));
        assert_eq!(
            std::fs::read(dir.path().join("account-lock/locks.json")).unwrap(),
            b"{broken"
        );
    }
}
