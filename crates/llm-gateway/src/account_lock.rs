//! session の開始 account と「跨いだ」印の置き場 (DR-0034)。
//!
//! thinking が account に束縛されるモデルの session は開始 account に結ばれる
//! (DR-0033)。その結びつきを失うと、跨いだ履歴の thinking が黙って落ちる。
//! restart と兄弟の unit を跨いで覚えておくため、`[stats] dir` の下の
//! `account-lock/locks.json` に 1 つの map として持ち、両 unit で共有する。
//!
//! - 読みはメモリの控えを引く。引く前にファイルの版 (mtime・inode・長さ) を
//!   見て、動いていれば読み直す
//! - 書きは状態が変わった時だけ ([`AccountLocks::remember`])。unit ごとに 1 本の
//!   書き手が順に、脇の `.lock` を flock で掴み、最新を読み直して当ててから
//!   丸ごと差し替える
//! - 読み書きの失敗は警告を残して進む。ロックは推論の連続性を守る仕組みで、
//!   置き場の障害で転送を止めるほど重くない

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::persist::write_atomically;
use crate::router::AccountLock;

/// 最後に通ってから、ロックを覚えておく時間 (ミリ秒)。
pub const LOCK_TTL_MS: i64 = 24 * 3600 * 1000;

/// `seen` だけが進んだ時に、ファイルへ書き直す間隔 (ミリ秒)。
///
/// `seen` の更新を 2xx ごとに書くと、書き込みがリクエスト数に比例する。
/// 寿命 24 時間に対して 5 分の誤差なら困らない。
const SEEN_STRIDE_MS: i64 = 300 * 1000;

/// ロックの鍵。affinity と同じ `(namespace 名, session key, モデル)`。
pub type Key = (String, String, String);

/// ファイルの 1 レコード。時刻は Unix ミリ秒。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    ns: String,
    session: String,
    model: String,
    account: String,
    crossed: bool,
    /// 開始 account が決まった時刻。2 つの言い分が食い違ったら早い方が勝つ。
    decided_at: i64,
    /// 最後にこの session が 2xx で通った時刻。寿命の起点。
    seen: i64,
}

impl Record {
    fn key(&self) -> Key {
        (self.ns.clone(), self.session.clone(), self.model.clone())
    }

    fn alive(&self, now: i64) -> bool {
        now - self.seen < LOCK_TTL_MS
    }

    /// 同じ鍵の別の言い分を当てる。`account` は先に決まった方 (同時なら
    /// self) が勝ち、`crossed` は OR、`seen` は大きい方 (DR-0034 決定 2)。
    ///
    /// 書き込みの順 (flock を取った順) で決めないのは、同じ unit の中でも
    /// unit の間でも、決まった順と書かれる順が入れ替わりうるため。
    ///
    /// 2 つの開始 account が違えば、遅く決めた側は勝った方の開始 account
    /// 以外へ送ったことになるので、それも跨ぎとして数える。
    fn absorb(&mut self, other: &Record) {
        self.crossed |= other.crossed || other.account != self.account;
        if other.decided_at < self.decided_at {
            self.account.clone_from(&other.account);
            self.decided_at = other.decided_at;
        }
        self.seen = self.seen.max(other.seen);
    }
}

/// ファイルの形。鍵の文字列は `[ns, session, model]` の JSON で、区切り文字の
/// 衝突が起きない。
type Map = HashMap<String, Record>;

fn map_key(key: &Key) -> String {
    serde_json::to_string(&[&key.0, &key.1, &key.2]).unwrap_or_default()
}

/// ファイルの版。rename のたびに inode が変わるので、同じ mtime (ナノ秒) の
/// 内に 2 度差し替わっても見分けられる。
type Version = (u128, u64, u64);

/// メモリの控え 1 件。
struct Entry {
    record: Record,
    /// ファイルに載っていると分かっている `seen`。まだ載っていなければ `None`。
    written: Option<i64>,
}

#[derive(Default)]
struct Memo {
    /// 控えが写しているファイルの版。
    version: Option<Version>,
    entries: HashMap<Key, Entry>,
}

impl Memo {
    /// 控えをファイルの中身に置き換える。ファイルが正で、残すのはまだ
    /// 書いていない手元の分だけ (書いた後にファイルから消えた鍵は、別の
    /// 書き手が刈ったもの)。同じ鍵に手元の言い分があれば当て、まだ書いて
    /// いないという印も引き継ぐ (次の 2xx で書き直すため)。
    fn replace_with(&mut self, map: Map, now: i64) {
        let mut previous = std::mem::take(&mut self.entries);
        for (_, mut from_file) in map {
            if !from_file.alive(now) {
                continue;
            }
            let key = from_file.key();
            let mut written = Some(from_file.seen);
            if let Some(mine) = previous.remove(&key) {
                written = mine.written.map(|w| w.max(from_file.seen));
                if mine.record.alive(now) {
                    from_file.absorb(&mine.record);
                }
            }
            self.entries.insert(
                key,
                Entry {
                    record: from_file,
                    written,
                },
            );
        }
        self.entries.extend(
            previous
                .into_iter()
                .filter(|(_, e)| e.written.is_none() && e.record.alive(now)),
        );
    }
}

/// 書き手への 1 件。
enum Job {
    Write(Record),
    /// ここまでの書き込みが済んだら知らせる。
    Drain(tokio::sync::oneshot::Sender<()>),
}

/// ロックの表。メモリの控えと、兄弟の unit と共有するファイル。
pub struct AccountLocks {
    path: PathBuf,
    lock_path: PathBuf,
    memo: std::sync::Mutex<Memo>,
    /// 裏の書き手への口。最初の書き込みで書き手を起こす (作る時点では
    /// runtime の内側とは限らない)。
    writer: OnceLock<mpsc::UnboundedSender<Job>>,
    /// ファイルへ書いた回数。試験で書き込みの合流を確かめるためだけに数える。
    #[cfg(test)]
    saves: std::sync::atomic::AtomicUsize,
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
            writer: OnceLock::new(),
            #[cfg(test)]
            saves: std::sync::atomic::AtomicUsize::new(0),
        };
        locks.refresh(&mut locks.memo(), crate::credential::time::now_unix_ms());
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

    /// `account` で 2xx が返ったことを覚える (`now` は Unix ミリ秒)。控えは
    /// その場で更新し、状態が変わっていればファイルへの書き込みを裏の書き手
    /// へ渡す (送信を待たせない)。
    pub fn remember(self: &Arc<Self>, key: Key, account: &str, now: i64) {
        if let Some(change) = self.note(key, account, now) {
            self.send(Job::Write(change));
        }
    }

    /// 書き手へ渡す。書き手は unit に 1 本で、受け取った順に書く — 書き込み
    /// ごとに並行させると flock を取る順が渡した順と入れ替わる。
    ///
    /// 溜まっていた分はまとめて 1 度の flock で当てて 1 度書く。1 件ごとに
    /// map 全体を書き直すと、一度に多くの session が始まった時に書き込みが
    /// その数だけ並ぶ。
    fn send(self: &Arc<Self>, job: Job) {
        let writer = self.writer.get_or_init(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
            // 書き手は表を弱く持つ。表が落ちれば口も閉じて書き手も終わる。
            let this: Weak<Self> = Arc::downgrade(self);
            tokio::spawn(async move {
                while let Some(first) = rx.recv().await {
                    let mut changes = Vec::new();
                    let mut drained = Vec::new();
                    let mut next = Some(first);
                    while let Some(job) = next {
                        match job {
                            Job::Write(change) => changes.push(change),
                            Job::Drain(done) => drained.push(done),
                        }
                        next = rx.try_recv().ok();
                    }
                    if !changes.is_empty() {
                        let Some(locks) = this.upgrade() else { break };
                        let written = tokio::task::spawn_blocking(move || {
                            locks.write(&changes, crate::credential::time::now_unix_ms());
                        })
                        .await;
                        if let Err(e) = written {
                            tracing::warn!(%e, "the session account lock writer failed");
                        }
                    }
                    for done in drained {
                        let _ = done.send(());
                    }
                }
            });
            tx
        });
        // 書き手が終わっているのは表が落ちる時だけで、書く先も無い。
        let _ = writer.send(job);
    }

    /// ここまでに渡した書き込みが済むまで、`limit` を上限に待つ。止まる前に
    /// 呼ぶ。間に合わなければ警告を残して諦める (次の起動で書き直す手段は
    /// 無いが、止まる側を待たせ続けるほどではない)。
    pub async fn drain(self: &Arc<Self>, limit: std::time::Duration) {
        if self.writer.get().is_none() {
            return;
        }
        let (done, wait) = tokio::sync::oneshot::channel();
        self.send(Job::Drain(done));
        if tokio::time::timeout(limit, wait).await.is_err() {
            tracing::warn!(path = %self.path.display(), "gave up waiting for the session account locks to be saved");
        }
    }

    /// 控えを更新し、ファイルへ書くべき変化なら書く中身を返す。
    ///
    /// 書くのは、開始 account が決まった時 / 印が立った時 / `seen` が前に
    /// 書いた値から [`SEEN_STRIDE_MS`] 以上進んだ時だけ (DR-0034 決定 4)。
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
                    .is_none_or(|written| now - written >= SEEN_STRIDE_MS);
                (entry.record.crossed != was_crossed || stale).then(|| entry.record.clone())
            }
            _ => {
                let record = Record {
                    ns: key.0.clone(),
                    session: key.1.clone(),
                    model: key.2.clone(),
                    account: account.to_owned(),
                    crossed: false,
                    decided_at: now,
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

    /// 版が動いていればファイルを読み直して控えを置き換える。ファイルが
    /// 消えていれば中身は空として置き換える (まだ書いていない手元の分だけ
    /// 残り、次の書き込みでファイルを作り直す)。
    fn refresh(&self, memo: &mut Memo, now: i64) {
        let version = self.version();
        if version == memo.version {
            return;
        }
        if version.is_none() {
            memo.replace_with(Map::new(), now);
            memo.version = None;
            return;
        }
        if let Some(map) = self.read() {
            memo.replace_with(map, now);
            memo.version = version;
        }
    }

    /// 更新時刻 (ナノ秒)・inode・長さを版として使う。書き換えは rename なので、
    /// 中身が入れ替われば inode が必ず変わる (DR-0010 の mtime に inode を足す)。
    fn version(&self) -> Option<Version> {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::metadata(&self.path).ok()?;
        let since_epoch = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some((since_epoch.as_nanos(), meta.ino(), meta.len()))
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
    /// 鍵が無い (または寿命切れ) なら入れる。あれば [`Record::absorb`] で
    /// 当てる。掴んでいる区間は読み直しから rename までだけ。
    fn write(&self, changes: &[Record], now: i64) {
        if let Err(e) = self.write_locked(changes, now) {
            tracing::warn!(path = %self.path.display(), %e, "cannot save the session account lock");
        }
    }

    fn write_locked(&self, changes: &[Record], now: i64) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _held = self.hold()?;
        // 読めない中身の上には書かない。書くと相手の開始 account を消しうる。
        let mut map = self
            .read()
            .ok_or_else(|| std::io::Error::other("the current locks are unreadable"))?;
        map.retain(|_, record| record.alive(now));
        for change in changes {
            let slot = map_key(&change.key());
            match map.get_mut(&slot) {
                Some(existing) => existing.absorb(change),
                None => {
                    map.insert(slot, change.clone());
                }
            }
        }
        write_atomically(&self.path, &map)?;
        #[cfg(test)]
        self.saves
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let version = self.version();
        let mut memo = self.memo();
        memo.replace_with(map, now);
        memo.version = version;
        // 書いた分は書けた印を付ける。手元でその後に進んだ分 (`seen`) は
        // 印の値より新しいので、間引きの判定で次に書かれる。
        for change in changes {
            if let Some(entry) = memo.entries.get_mut(&change.key()) {
                entry.written = Some(entry.written.map_or(change.seen, |w| w.max(change.seen)));
            }
        }
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

    /// ここまでに渡した書き込みが全部終わるまで待つ。
    #[cfg(test)]
    pub async fn settled(self: &Arc<Self>) {
        self.drain(std::time::Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_800_000_000_000;

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
                locks.write(std::slice::from_ref(&change), now);
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

    /// 並行した初回: 控えは互いに知らず、flock を取る順が決まった順と逆でも、
    /// 先に決まった方の account が開始 account になる。
    #[test]
    fn the_earlier_decision_wins_whatever_order_it_is_written_in() {
        let dir = tempfile::tempdir().unwrap();
        let stable = AccountLocks::open(dir.path());
        let unstable = AccountLocks::open(dir.path());
        let first = stable.note(key("s"), "a", T0).unwrap();
        let second = unstable.note(key("s"), "b", T0 + 1).unwrap();

        unstable.write(std::slice::from_ref(&second), T0 + 1);
        stable.write(std::slice::from_ref(&first), T0 + 1);

        assert_eq!(stable.get(&key("s"), T0 + 1), lock("a", true));
        assert_eq!(unstable.get(&key("s"), T0 + 1), lock("a", true));
    }

    /// 同じ unit の書き込みは渡した順に 1 本ずつ流れる。
    #[tokio::test]
    async fn one_unit_writes_in_the_order_it_decided() {
        let dir = tempfile::tempdir().unwrap();
        let locks = Arc::new(AccountLocks::open(dir.path()));
        for i in 0..20 {
            locks.remember(key(&format!("s{i}")), "a", T0 + i);
        }
        locks.remember(key("s0"), "b", T0 + 100);
        locks.settled().await;

        let disk = on_disk(dir.path());
        assert_eq!(disk.len(), 20);
        let record = &disk[&map_key(&key("s0"))];
        assert_eq!((record.account.as_str(), record.crossed), ("a", true));
    }

    /// 同じ mtime のまま差し替わっても (rename で inode が変わるので) 読み直す。
    #[test]
    fn a_replacement_within_the_same_mtime_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account-lock/locks.json");
        let pin = |path: &Path| {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
            )
            .unwrap();
        };
        let writer = AccountLocks::open(dir.path());
        pass(&writer, "s1", "a", T0);
        pin(&path);
        let reader = AccountLocks::open(dir.path());
        assert_eq!(reader.get(&key("s1"), T0), lock("a", false));

        // 同じ長さ・同じ mtime の中身へ差し替える。
        let mut map = on_disk(dir.path());
        let mut record = map.remove(&map_key(&key("s1"))).unwrap();
        record.session = "s2".into();
        map.insert(map_key(&key("s2")), record);
        write_atomically(&path, &map).unwrap();
        pin(&path);

        assert_eq!(reader.get(&key("s2"), T0), lock("a", false));
        assert_eq!(reader.get(&key("s1"), T0), None, "the file is the truth");
    }

    /// 読み直しは控えをファイルの中身に置き換える。書いた後にファイルから
    /// 消えた鍵 (別の書き手が刈った) は落とし、まだ書いていない鍵は残す。
    #[test]
    fn a_refresh_drops_keys_gone_from_the_file_but_keeps_unwritten_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account-lock/locks.json");
        let locks = AccountLocks::open(dir.path());
        pass(&locks, "gone", "a", T0);
        pass(&locks, "kept", "a", T0);
        // 控えにだけあって、まだ書いていない鍵。
        locks.note(key("pending"), "a", T0).unwrap();

        // 別の書き手が "gone" を刈った。
        let mut map = on_disk(dir.path());
        map.remove(&map_key(&key("gone")));
        write_atomically(&path, &map).unwrap();

        assert_eq!(locks.get(&key("gone"), T0), None);
        assert_eq!(locks.get(&key("kept"), T0), lock("a", false));
        assert_eq!(locks.get(&key("pending"), T0), lock("a", false));
    }

    /// 寿命切れは読み手に見えず、次の書き込みで消える。
    #[test]
    fn expired_locks_are_invisible_and_pruned_on_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let locks = AccountLocks::open(dir.path());
        pass(&locks, "old", "a", T0);
        let later = T0 + LOCK_TTL_MS;

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
        pass(&locks, "s", "b", T0 + LOCK_TTL_MS);
        assert_eq!(locks.get(&key("s"), T0 + LOCK_TTL_MS), lock("b", false));
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
        assert!(!pass(&locks, "s", "a", T0 + SEEN_STRIDE_MS - 1));
        assert_eq!(on_disk(dir.path())[&map_key(&key("s"))].seen, T0);

        assert!(pass(&locks, "s", "a", T0 + SEEN_STRIDE_MS));
        assert!(
            pass(&locks, "s", "b", T0 + SEEN_STRIDE_MS + 1),
            "crossing is written"
        );
        assert!(!pass(&locks, "s", "b", T0 + SEEN_STRIDE_MS + 2));
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

    /// 止まる前の drain で、渡しただけの書き込みもファイルに載る。
    #[tokio::test]
    async fn a_drain_writes_what_was_queued() {
        let dir = tempfile::tempdir().unwrap();
        let locks = Arc::new(AccountLocks::open(dir.path()));
        locks.remember(key("s1"), "a", T0);
        locks.remember(key("s2"), "b", T0);
        locks.drain(std::time::Duration::from_secs(5)).await;

        let disk = on_disk(dir.path());
        assert!(disk.contains_key(&map_key(&key("s1"))));
        assert!(disk.contains_key(&map_key(&key("s2"))));
    }

    /// 溜まった書き込みは 1 度の flock でまとめて書く。
    #[tokio::test]
    async fn queued_writes_are_merged_into_one_save() {
        let dir = tempfile::tempdir().unwrap();
        let locks = Arc::new(AccountLocks::open(dir.path()));
        for i in 0..10 {
            locks.remember(key(&format!("s{i}")), "a", T0 + i);
        }
        locks.settled().await;

        assert_eq!(on_disk(dir.path()).len(), 10);
        let saves = locks.saves.load(std::sync::atomic::Ordering::Relaxed);
        assert!((1..=2).contains(&saves), "saved {saves} times");
    }

    /// 読み直しで、まだ書いていない手元の分はその印ごと残り、次の 2xx で書く。
    #[test]
    fn an_unwritten_lock_stays_pending_across_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let mine = AccountLocks::open(dir.path());
        let sibling = AccountLocks::open(dir.path());
        // 手元で決まったが、まだ書いていない。
        mine.note(key("s"), "a", T0).unwrap();
        // その間に兄弟が同じ session を後から決めて書いた。
        pass(&sibling, "s", "b", T0 + 1);

        assert_eq!(mine.get(&key("s"), T0 + 2), lock("a", true));
        assert!(
            pass(&mine, "s", "a", T0 + 3),
            "the pending decision is retried on the next success"
        );
        assert_eq!(sibling.get(&key("s"), T0 + 4), lock("a", true));
    }

    /// ファイルが消えたら、読み直しでロックも消える (次の書き込みで作り直す)。
    #[test]
    fn a_removed_file_forgets_the_locks() {
        let dir = tempfile::tempdir().unwrap();
        let locks = AccountLocks::open(dir.path());
        pass(&locks, "s", "a", T0);
        std::fs::remove_file(dir.path().join("account-lock/locks.json")).unwrap();

        assert_eq!(locks.get(&key("s"), T0), None);
        assert!(pass(&locks, "s", "b", T0 + 1));
        assert_eq!(on_disk(dir.path())[&map_key(&key("s"))].account, "b");
    }
}
