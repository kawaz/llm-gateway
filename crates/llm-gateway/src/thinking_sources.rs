//! session の thinking の出所 (account, model) の置き場 (DR-0035 §2・§6)。
//!
//! thinking は生成した model でしか読めず、model によっては生成した account
//! でしか読めない。跨いだ履歴の thinking は黙って落ちるので、session ごとに
//! 「2xx で通った (account, model)」を覚えておき、送る 1 本が跨ぐかを判定する。
//! restart と兄弟の unit を跨いで覚えておくため、`[stats] dir` の下の
//! `thinking-sources/sources.json` に 1 つの map として持ち、両 unit で共有する。
//!
//! - 読みはメモリの控えを引く。引く前にファイルの版 (mtime・inode・長さ) を
//!   見て、動いていれば読み直す
//! - 書きは状態が変わった時だけ ([`ThinkingSources::remember`])。unit ごとに 1 本の
//!   書き手が順に、脇の `.lock` を flock で掴み、最新を読み直して当ててから
//!   丸ごと差し替える
//! - 読み書きの失敗は警告を残して進む。出所は推論の連続性を守る仕組みで、
//!   置き場の障害で転送を止めるほど重くない

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::persist::write_atomically;

/// session が最後に通ってから、出所を覚えておく時間 (ミリ秒)。
pub const SOURCE_TTL_MS: i64 = 24 * 3600 * 1000;

/// `seen` だけが進んだ時に、ファイルへ書き直す間隔 (ミリ秒)。
///
/// `seen` の更新を 2xx ごとに書くと、書き込みがリクエスト数に比例する。
/// 寿命 24 時間に対して 5 分の誤差なら困らない。
const SEEN_STRIDE_MS: i64 = 300 * 1000;

/// session の中で出所を見分ける鍵。`(account, model)`。
type Tuple = (String, String);

/// ファイルの 1 レコード = 出所 1 つ。時刻は Unix ミリ秒。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    session: String,
    account: String,
    model: String,
    /// この出所が最初に決まった時刻。開始 account を決める。
    decided_at: i64,
    /// 最後にこの session が 2xx で通った時刻。寿命の起点。
    seen: i64,
}

impl Record {
    fn tuple(&self) -> Tuple {
        (self.account.clone(), self.model.clone())
    }

    fn alive(&self, now: i64) -> bool {
        now - self.seen < SOURCE_TTL_MS
    }

    /// 同じ鍵の別の言い分を当てる。`decided_at` は早い方、`seen` は遅い方
    /// (DR-0035 §2)。鍵に account を含むので、取り合う欄は無い。
    fn absorb(&mut self, other: &Record) {
        self.decided_at = self.decided_at.min(other.decided_at);
        self.seen = self.seen.max(other.seen);
    }

    /// ファイルの map の鍵。`[session, account, model]` の JSON で、区切り文字の
    /// 衝突が起きない。
    fn map_key(&self) -> String {
        serde_json::to_string(&[&self.session, &self.account, &self.model]).unwrap_or_default()
    }
}

type Map = HashMap<String, Record>;

/// ファイルの版。rename のたびに inode が変わるので、同じ mtime (ナノ秒) の
/// 内に 2 度差し替わっても見分けられる。
type Version = (u128, u64, u64);

/// 出所 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub account: String,
    /// 解決後のモデル名そのまま (date 接尾辞や family で寄せない)。
    pub model: String,
    pub decided_at: i64,
}

/// session の thinking の出所の集合 (DR-0035 §2)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSources(Vec<Source>);

impl FromIterator<Source> for SessionSources {
    fn from_iter<I: IntoIterator<Item = Source>>(iter: I) -> Self {
        let mut sources: Vec<Source> = iter.into_iter().collect();
        // 並びを固定する。比べる側 (試験・判定) が控えの HashMap の順に左右されない。
        sources.sort_by(|x, y| {
            (x.decided_at, &x.account, &x.model).cmp(&(y.decided_at, &y.account, &y.model))
        });
        Self(sources)
    }
}

impl SessionSources {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// この model の開始 account。同じ model の出所のうち最初に決まったもの。
    /// 同時なら account 名の小さい方 (どの unit でも同じ答えにするため)。
    pub fn starting_account(&self, model: &str) -> Option<&str> {
        self.0
            .iter()
            .filter(|source| source.model == model)
            .min_by(|x, y| (x.decided_at, &x.account).cmp(&(y.decided_at, &y.account)))
            .map(|source| source.account.as_str())
    }

    /// `model` を `account` へ送る 1 本が、履歴のどれかの thinking を読めないか
    /// (DR-0035 §1)。model が違えば必ず、account 束縛の model なら account が
    /// 違っても読めない。
    pub fn crossed(&self, model: &str, account: &str, account_bound: bool) -> bool {
        self.0
            .iter()
            .any(|source| source.model != model || (account_bound && source.account != account))
    }
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
    version: Option<Version>,
    /// session で引く。リクエストごとの lookup を全件走査にしないため。
    sessions: HashMap<String, HashMap<Tuple, Entry>>,
}

impl Memo {
    fn put(&mut self, record: Record, written: Option<i64>) {
        self.sessions
            .entry(record.session.clone())
            .or_default()
            .insert(record.tuple(), Entry { record, written });
    }

    /// 控えをファイルの中身に置き換える。ファイルが正で、残すのはまだ
    /// 書いていない手元の分だけ (書いた後にファイルから消えた鍵は、別の
    /// 書き手が刈ったもの)。同じ鍵に手元の言い分があれば当て、まだ書いて
    /// いないという印も引き継ぐ (次の 2xx で書き直すため)。
    fn replace_with(&mut self, map: Map, now: i64) {
        let mut previous = std::mem::take(&mut self.sessions);
        for (_, mut from_file) in map {
            if !from_file.alive(now) {
                continue;
            }
            let mut written = Some(from_file.seen);
            let mine = previous
                .get_mut(&from_file.session)
                .and_then(|tuples| tuples.remove(&from_file.tuple()));
            if let Some(mine) = mine {
                written = mine.written.map(|w| w.max(from_file.seen));
                if mine.record.alive(now) {
                    from_file.absorb(&mine.record);
                }
            }
            self.put(from_file, written);
        }
        for entry in previous.into_values().flat_map(HashMap::into_values) {
            if entry.written.is_none() && entry.record.alive(now) {
                self.put(entry.record, None);
            }
        }
    }
}

/// 書き手への 1 件。
enum Job {
    Write(Vec<Record>),
    /// ここまでの書き込みが済んだら知らせる。
    Drain(tokio::sync::oneshot::Sender<()>),
}

/// 出所の表。メモリの控えと、兄弟の unit と共有するファイル。
pub struct ThinkingSources {
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

impl ThinkingSources {
    /// `[stats] dir` の下、`thinking-sources/` に置く。今の中身を読んで控えに載せる
    /// (刈り込みはしない、寿命切れは読む側で無視する)。
    pub fn open(stats_dir: impl AsRef<Path>) -> Self {
        let dir = stats_dir.as_ref().join("thinking-sources");
        let sources = Self {
            path: dir.join("sources.json"),
            lock_path: dir.join("sources.json.lock"),
            memo: std::sync::Mutex::new(Memo::default()),
            writer: OnceLock::new(),
            #[cfg(test)]
            saves: std::sync::atomic::AtomicUsize::new(0),
        };
        sources.refresh(&mut sources.memo(), crate::credential::time::now_unix_ms());
        sources
    }

    fn memo(&self) -> std::sync::MutexGuard<'_, Memo> {
        self.memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// この session の出所。寿命切れは含めない。
    pub fn get(&self, session: &str, now: i64) -> SessionSources {
        let mut memo = self.memo();
        self.refresh(&mut memo, now);
        memo.sessions
            .get(session)
            .into_iter()
            .flat_map(HashMap::values)
            .filter(|entry| entry.record.alive(now))
            .map(|entry| Source {
                account: entry.record.account.clone(),
                model: entry.record.model.clone(),
                decided_at: entry.record.decided_at,
            })
            .collect()
    }

    /// `session` が `account` の `model` で 2xx を得たことを覚える (`now` は
    /// Unix ミリ秒)。控えはその場で更新し、書くべき変化ならファイルへの書き込みを
    /// 裏の書き手へ渡す (送信を待たせない)。
    pub fn remember(self: &Arc<Self>, session: &str, account: &str, model: &str, now: i64) {
        if let Some(changes) = self.note(session, account, model, now) {
            self.send(Job::Write(changes));
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
                            Job::Write(more) => changes.extend(more),
                            Job::Drain(done) => drained.push(done),
                        }
                        next = rx.try_recv().ok();
                    }
                    if !changes.is_empty() {
                        let Some(sources) = this.upgrade() else { break };
                        let written = tokio::task::spawn_blocking(move || {
                            sources.write(&changes, crate::credential::time::now_unix_ms());
                        })
                        .await;
                        if let Err(e) = written {
                            tracing::warn!(%e, "the thinking sources writer failed");
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
            tracing::warn!(path = %self.path.display(), "gave up waiting for the thinking sources to be saved");
        }
    }

    /// 控えを更新し、ファイルへ書くべき変化なら書く中身 (その session の出所
    /// 全部) を返す。
    ///
    /// `seen` は session 単位で進める: どの出所で通っても全部の寿命を延ばす
    /// (跨ぎの記憶が session の活動中に先に切れないため、DR-0035 §2)。書くのは
    /// 新しい出所が入った時か、`seen` が前に書いた値から [`SEEN_STRIDE_MS`]
    /// 以上進んだ時だけ。
    fn note(&self, session: &str, account: &str, model: &str, now: i64) -> Option<Vec<Record>> {
        let mut memo = self.memo();
        self.refresh(&mut memo, now);
        let tuples = memo.sessions.entry(session.to_owned()).or_default();
        tuples.retain(|_, entry| entry.record.alive(now));
        tuples
            .entry((account.to_owned(), model.to_owned()))
            .or_insert_with(|| Entry {
                record: Record {
                    session: session.to_owned(),
                    account: account.to_owned(),
                    model: model.to_owned(),
                    decided_at: now,
                    seen: now,
                },
                written: None,
            });
        for entry in tuples.values_mut() {
            entry.record.seen = entry.record.seen.max(now);
        }
        let due = tuples.values().any(|entry| {
            entry
                .written
                .is_none_or(|written| now - written >= SEEN_STRIDE_MS)
        });
        due.then(|| tuples.values().map(|entry| entry.record.clone()).collect())
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
                tracing::warn!(path = %self.path.display(), %e, "cannot read the thinking sources");
                return None;
            }
        };
        match serde_json::from_slice(&raw) {
            Ok(map) => Some(map),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), %e, "the thinking sources are unreadable");
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
            tracing::warn!(path = %self.path.display(), %e, "cannot save the thinking sources");
        }
    }

    fn write_locked(&self, changes: &[Record], now: i64) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _held = self.hold()?;
        // 読めない中身の上には書かない。書くと相手の出所を消しうる。
        let mut map = self
            .read()
            .ok_or_else(|| std::io::Error::other("the current thinking sources are unreadable"))?;
        map.retain(|_, record| record.alive(now));
        for change in changes {
            match map.get_mut(&change.map_key()) {
                Some(existing) => existing.absorb(change),
                None => {
                    map.insert(change.map_key(), change.clone());
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
            let entry = memo
                .sessions
                .get_mut(&change.session)
                .and_then(|tuples| tuples.get_mut(&change.tuple()));
            if let Some(entry) = entry {
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

    fn source(account: &str, model: &str, decided_at: i64) -> Source {
        Source {
            account: account.to_owned(),
            model: model.to_owned(),
            decided_at,
        }
    }

    fn sources(list: &[(&str, &str, i64)]) -> SessionSources {
        list.iter()
            .map(|(account, model, at)| source(account, model, *at))
            .collect()
    }

    /// 書き込みを同期で当てる (裏の仕事を介さない)。
    fn pass(table: &ThinkingSources, s: &str, account: &str, model: &str, now: i64) -> bool {
        match table.note(s, account, model, now) {
            Some(changes) => {
                table.write(&changes, now);
                true
            }
            None => false,
        }
    }

    fn on_disk(dir: &Path) -> Map {
        let raw = std::fs::read(dir.join("thinking-sources/sources.json")).unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    fn disk_key(s: &str, account: &str, model: &str) -> String {
        serde_json::to_string(&[s, account, model]).unwrap()
    }

    #[test]
    fn the_file_holds_one_record_per_tuple() {
        let dir = tempfile::tempdir().unwrap();
        let table = ThinkingSources::open(dir.path());
        pass(&table, "s", "a", "m", T0);
        let raw: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("thinking-sources/sources.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            raw,
            serde_json::json!({
                (disk_key("s", "a", "m")): {
                    "session": "s", "account": "a", "model": "m",
                    "decided_at": T0, "seen": T0,
                },
            })
        );
        assert!(
            dir.path()
                .join("thinking-sources/sources.json.lock")
                .exists()
        );
    }

    /// 別 account・別 model はそれぞれ別のレコード。
    #[test]
    fn another_account_or_model_is_another_record() {
        let dir = tempfile::tempdir().unwrap();
        let table = ThinkingSources::open(dir.path());
        pass(&table, "s", "a", "m", T0);
        pass(&table, "s", "b", "m", T0 + 1);
        pass(&table, "s", "a", "n", T0 + 2);

        assert_eq!(
            table.get("s", T0 + 3),
            sources(&[("a", "m", T0), ("b", "m", T0 + 1), ("a", "n", T0 + 2)])
        );
        assert_eq!(on_disk(dir.path()).len(), 3);
        assert!(table.get("other", T0 + 3).is_empty());
    }

    /// 2 つの unit が同じ置き場を見て、同じ鍵の言い分は `decided_at` = min、
    /// `seen` = max で当たる。書く順が決まった順と逆でも同じ。
    #[test]
    fn two_units_merge_the_same_key_by_min_and_max() {
        let dir = tempfile::tempdir().unwrap();
        let stable = ThinkingSources::open(dir.path());
        let unstable = ThinkingSources::open(dir.path());
        let early = stable.note("s", "a", "m", T0).unwrap();
        let late = unstable.note("s", "a", "m", T0 + 5).unwrap();

        unstable.write(&late, T0 + 5);
        stable.write(&early, T0 + 5);

        let record = &on_disk(dir.path())[&disk_key("s", "a", "m")];
        assert_eq!((record.decided_at, record.seen), (T0, T0 + 5));
        assert_eq!(stable.get("s", T0 + 6), sources(&[("a", "m", T0)]));
        assert_eq!(unstable.get("s", T0 + 6), sources(&[("a", "m", T0)]));
    }

    /// 2 つの unit が別 account で通れば、両方の出所が残る。開始 account は
    /// 先に決まった方。
    #[test]
    fn two_units_keep_both_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let stable = ThinkingSources::open(dir.path());
        let unstable = ThinkingSources::open(dir.path());
        let first = stable.note("s", "a", "m", T0).unwrap();
        let second = unstable.note("s", "b", "m", T0 + 1).unwrap();

        unstable.write(&second, T0 + 1);
        stable.write(&first, T0 + 1);

        for table in [&stable, &unstable] {
            let got = table.get("s", T0 + 2);
            assert_eq!(got, sources(&[("a", "m", T0), ("b", "m", T0 + 1)]));
            assert_eq!(got.starting_account("m"), Some("a"));
        }
    }

    /// 同時に決まったら、どの unit でも account 名の小さい方が開始 account。
    #[test]
    fn a_tie_is_broken_by_the_account_name() {
        let got = sources(&[("b", "m", T0), ("a", "m", T0), ("c", "n", T0 - 1)]);
        assert_eq!(got.starting_account("m"), Some("a"));
        assert_eq!(got.starting_account("n"), Some("c"));
        assert_eq!(got.starting_account("x"), None);
    }

    #[test]
    fn crossing_is_a_foreign_model_or_a_foreign_account_of_a_bound_model() {
        let got = sources(&[("a", "m", T0)]);
        assert!(!got.crossed("m", "a", true));
        assert!(
            !got.crossed("m", "b", false),
            "the account binds only bound models"
        );
        assert!(got.crossed("m", "b", true));
        assert!(
            got.crossed("n", "a", false),
            "every model binds its thinking"
        );
        assert!(!SessionSources::default().crossed("m", "b", true));
    }

    /// session のどれかの出所で通れば、その session の全部の `seen` が進む。
    #[test]
    fn activity_on_one_tuple_extends_the_whole_session() {
        let dir = tempfile::tempdir().unwrap();
        let table = ThinkingSources::open(dir.path());
        pass(&table, "s", "a", "m", T0);
        pass(&table, "s", "a", "n", T0 + SOURCE_TTL_MS / 2);
        let later = T0 + SOURCE_TTL_MS + 1;
        assert!(pass(&table, "s", "a", "n", later - 2));

        assert_eq!(
            table.get("s", later),
            sources(&[("a", "m", T0), ("a", "n", T0 + SOURCE_TTL_MS / 2)]),
            "the first tuple outlives its own last success"
        );
        let disk = on_disk(dir.path());
        assert_eq!(disk[&disk_key("s", "a", "m")].seen, later - 2);
        assert_eq!(disk[&disk_key("s", "a", "n")].seen, later - 2);
    }

    /// 同じ mtime のまま差し替わっても (rename で inode が変わるので) 読み直す。
    #[test]
    fn a_replacement_within_the_same_mtime_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinking-sources/sources.json");
        let pin = |path: &Path| {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
            )
            .unwrap();
        };
        let writer = ThinkingSources::open(dir.path());
        pass(&writer, "s1", "a", "m", T0);
        pin(&path);
        let reader = ThinkingSources::open(dir.path());
        assert_eq!(reader.get("s1", T0), sources(&[("a", "m", T0)]));

        // 同じ長さ・同じ mtime の中身へ差し替える。
        let mut map = on_disk(dir.path());
        let mut record = map.remove(&disk_key("s1", "a", "m")).unwrap();
        record.session = "s2".into();
        map.insert(record.map_key(), record);
        write_atomically(&path, &map).unwrap();
        pin(&path);

        assert_eq!(reader.get("s2", T0), sources(&[("a", "m", T0)]));
        assert!(reader.get("s1", T0).is_empty(), "the file is the truth");
    }

    /// 読み直しは控えをファイルの中身に置き換える。書いた後にファイルから
    /// 消えた鍵 (別の書き手が刈った) は落とし、まだ書いていない鍵は残す。
    #[test]
    fn a_refresh_drops_keys_gone_from_the_file_but_keeps_unwritten_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinking-sources/sources.json");
        let table = ThinkingSources::open(dir.path());
        pass(&table, "gone", "a", "m", T0);
        pass(&table, "kept", "a", "m", T0);
        // 控えにだけあって、まだ書いていない鍵。
        table.note("pending", "a", "m", T0).unwrap();

        // 別の書き手が "gone" を刈った。
        let mut map = on_disk(dir.path());
        map.remove(&disk_key("gone", "a", "m"));
        write_atomically(&path, &map).unwrap();

        assert!(table.get("gone", T0).is_empty());
        assert_eq!(table.get("kept", T0), sources(&[("a", "m", T0)]));
        assert_eq!(table.get("pending", T0), sources(&[("a", "m", T0)]));
    }

    /// 寿命切れは読み手に見えず、次の書き込みで消える。
    #[test]
    fn expired_sources_are_invisible_and_pruned_on_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let table = ThinkingSources::open(dir.path());
        pass(&table, "old", "a", "m", T0);
        let later = T0 + SOURCE_TTL_MS;

        let fresh = ThinkingSources::open(dir.path());
        assert!(fresh.get("old", later).is_empty());
        assert!(table.get("old", later).is_empty());
        assert!(
            on_disk(dir.path()).contains_key(&disk_key("old", "a", "m")),
            "reading does not prune"
        );

        pass(&fresh, "new", "b", "m", later);
        let disk = on_disk(dir.path());
        assert!(!disk.contains_key(&disk_key("old", "a", "m")));
        assert!(disk.contains_key(&disk_key("new", "b", "m")));
    }

    /// 寿命切れの後に通ったら、その出所だけで始め直す。
    #[test]
    fn an_expired_session_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let table = ThinkingSources::open(dir.path());
        pass(&table, "s", "a", "m", T0);
        pass(&table, "s", "b", "m", T0 + SOURCE_TTL_MS);
        let got = table.get("s", T0 + SOURCE_TTL_MS);
        assert_eq!(got, sources(&[("b", "m", T0 + SOURCE_TTL_MS)]));
        assert_eq!(got.starting_account("m"), Some("b"));
    }

    /// `seen` だけが進んだ時は 5 分ごとにしか書かない。新しい出所はすぐ書く。
    #[test]
    fn only_new_tuples_and_a_stale_seen_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let table = ThinkingSources::open(dir.path());
        assert!(pass(&table, "s", "a", "m", T0), "a new tuple is written");
        assert!(!pass(&table, "s", "a", "m", T0 + SEEN_STRIDE_MS - 1));
        assert_eq!(on_disk(dir.path())[&disk_key("s", "a", "m")].seen, T0);

        assert!(pass(&table, "s", "a", "m", T0 + SEEN_STRIDE_MS));
        assert!(
            pass(&table, "s", "b", "m", T0 + SEEN_STRIDE_MS + 1),
            "another account is written at once"
        );
        assert!(!pass(&table, "s", "a", "m", T0 + SEEN_STRIDE_MS + 2));
        assert!(
            pass(&table, "s", "a", "n", T0 + SEEN_STRIDE_MS + 3),
            "another model is written at once"
        );
    }

    /// 書けなかった出所は、次の 2xx で書き直す。
    #[test]
    fn a_failed_write_is_retried_on_the_next_success() {
        let dir = tempfile::tempdir().unwrap();
        // 置き場になるはずの場所をファイルで塞ぐ。
        std::fs::write(dir.path().join("thinking-sources"), b"").unwrap();
        let table = ThinkingSources::open(dir.path());
        assert!(pass(&table, "s", "a", "m", T0));
        assert_eq!(
            table.get("s", T0),
            sources(&[("a", "m", T0)]),
            "the memo still holds it"
        );

        std::fs::remove_file(dir.path().join("thinking-sources")).unwrap();
        assert!(pass(&table, "s", "a", "m", T0 + 1));
        assert_eq!(on_disk(dir.path())[&disk_key("s", "a", "m")].decided_at, T0);
    }

    /// 読めないファイルがあっても転送は止めず、上書きもしない。
    #[test]
    fn an_unreadable_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("thinking-sources")).unwrap();
        std::fs::write(dir.path().join("thinking-sources/sources.json"), b"{broken").unwrap();
        let table = ThinkingSources::open(dir.path());
        pass(&table, "s", "a", "m", T0);
        assert_eq!(table.get("s", T0), sources(&[("a", "m", T0)]));
        assert_eq!(
            std::fs::read(dir.path().join("thinking-sources/sources.json")).unwrap(),
            b"{broken"
        );
    }

    /// 同じ unit の書き込みは渡した順に 1 本ずつ流れる。
    #[tokio::test]
    async fn one_unit_writes_in_the_order_it_decided() {
        let dir = tempfile::tempdir().unwrap();
        let table = Arc::new(ThinkingSources::open(dir.path()));
        for i in 0..20 {
            table.remember(&format!("s{i}"), "a", "m", T0 + i);
        }
        table.remember("s0", "b", "m", T0 + 100);
        table.settled().await;

        let disk = on_disk(dir.path());
        assert_eq!(disk.len(), 21);
        assert_eq!(disk[&disk_key("s0", "a", "m")].seen, T0 + 100);
        assert_eq!(disk[&disk_key("s0", "b", "m")].decided_at, T0 + 100);
    }

    /// 止まる前の drain で、渡しただけの書き込みもファイルに載る。
    #[tokio::test]
    async fn a_drain_writes_what_was_queued() {
        let dir = tempfile::tempdir().unwrap();
        let table = Arc::new(ThinkingSources::open(dir.path()));
        table.remember("s1", "a", "m", T0);
        table.remember("s2", "b", "m", T0);
        table.drain(std::time::Duration::from_secs(5)).await;

        let disk = on_disk(dir.path());
        assert!(disk.contains_key(&disk_key("s1", "a", "m")));
        assert!(disk.contains_key(&disk_key("s2", "b", "m")));
    }

    /// 溜まった書き込みは 1 度の flock でまとめて書く。
    #[tokio::test]
    async fn queued_writes_are_merged_into_one_save() {
        let dir = tempfile::tempdir().unwrap();
        let table = Arc::new(ThinkingSources::open(dir.path()));
        for i in 0..10 {
            table.remember(&format!("s{i}"), "a", "m", T0 + i);
        }
        table.settled().await;

        assert_eq!(on_disk(dir.path()).len(), 10);
        let saves = table.saves.load(std::sync::atomic::Ordering::Relaxed);
        assert!((1..=2).contains(&saves), "saved {saves} times");
    }

    /// 読み直しで、まだ書いていない手元の分はその印ごと残り、次の 2xx で書く。
    #[test]
    fn an_unwritten_tuple_stays_pending_across_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let mine = ThinkingSources::open(dir.path());
        let sibling = ThinkingSources::open(dir.path());
        // 手元で決まったが、まだ書いていない。
        mine.note("s", "a", "m", T0).unwrap();
        // その間に兄弟が同じ session を別 account で書いた。
        pass(&sibling, "s", "b", "m", T0 + 1);

        assert_eq!(
            mine.get("s", T0 + 2),
            sources(&[("a", "m", T0), ("b", "m", T0 + 1)])
        );
        assert!(
            pass(&mine, "s", "a", "m", T0 + 3),
            "the pending tuple is retried on the next success"
        );
        let got = sibling.get("s", T0 + 4);
        assert_eq!(got, sources(&[("a", "m", T0), ("b", "m", T0 + 1)]));
        assert_eq!(got.starting_account("m"), Some("a"));
    }

    /// ファイルが消えたら、読み直しで出所も消える (次の書き込みで作り直す)。
    #[test]
    fn a_removed_file_forgets_the_sources() {
        let dir = tempfile::tempdir().unwrap();
        let table = ThinkingSources::open(dir.path());
        pass(&table, "s", "a", "m", T0);
        std::fs::remove_file(dir.path().join("thinking-sources/sources.json")).unwrap();

        assert!(table.get("s", T0).is_empty());
        assert!(pass(&table, "s", "b", "m", T0 + 1));
        assert_eq!(table.get("s", T0 + 1), sources(&[("b", "m", T0 + 1)]));
        assert_eq!(on_disk(dir.path()).len(), 1);
    }

    /// 作り直した表 (restart 相当) が、ファイルから出所を引き継ぐ。
    #[test]
    fn a_reopened_table_reads_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let table = ThinkingSources::open(dir.path());
        pass(&table, "s", "a", "m", T0);
        pass(&table, "s", "b", "n", T0 + 1);
        drop(table);

        let reopened = ThinkingSources::open(dir.path());
        assert_eq!(
            reopened.get("s", T0 + 2),
            sources(&[("a", "m", T0), ("b", "n", T0 + 1)])
        );
    }
}
