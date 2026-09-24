//! 平文ファイルへの保存。
//!
//! 1 認証情報 1 ファイルの JSON (値の形は利用側が決める)。ファイル名の
//! stem がそのまま識別子になる。
//!
//! 平文なので、同じ UID のプロセスなら誰でも読める。パーミッションは絞るが
//! それ以上の保護は無い。暗号化やアクセス制御は別の置き場の実装で埋める。

use std::fs;
use std::io::Write as _;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Error, Result};

use super::{CredentialId, Persistence};

/// ディレクトリ 1 つを認証情報の置き場にする。`V` は 1 ファイルに置く値。
pub struct FileStore<V> {
    dir: PathBuf,
    value: PhantomData<fn() -> V>,
}

impl<V> FileStore<V> {
    /// 置き場を指定して開く。無ければ作る。
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        restrict(&dir, 0o700)?;
        Ok(Self {
            dir,
            value: PhantomData,
        })
    }

    fn path_of(&self, id: &CredentialId) -> PathBuf {
        self.dir.join(format!("{}.json", id.as_str()))
    }

    /// 排他に使う脇のファイル。中身は空で、開けること自体に意味がある。
    fn lock_path_of(&self, id: &CredentialId) -> PathBuf {
        self.dir.join(format!("{}.json.lock", id.as_str()))
    }
}

/// 書き換えの権利。落とすと flock が外れ、待っている相手が起きる。
pub struct FileGuard {
    id: CredentialId,
    /// 開いたまま抱えておくためだけに持つ。閉じるとロックが外れる。
    _file: fs::File,
}

impl<V> Persistence for FileStore<V>
where
    V: Serialize + DeserializeOwned + 'static,
{
    type Value = V;
    type Guard = FileGuard;

    /// 脇の `.lock` ファイルを掴む。掴めるまで待つ。
    ///
    /// ロックを認証情報そのものに付けない。書き換えは rename で行うので、
    /// 掴んだファイルは書き換えの瞬間に古い中身のほうへ取り残され、後から
    /// 来た相手は別のファイルを掴んで素通りする。名前が動かない脇の
    /// ファイルなら、全員が同じ 1 つを待つ。この `.lock` は消さない
    /// (消して作り直すと、既に掴んでいる側と後から来た側が別のファイルを
    /// 見ることになり、同じ理由で締め出しが破れる)。
    fn lock(&self, id: &CredentialId) -> Result<Self::Guard> {
        let path = self.lock_path_of(id);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        restrict(&path, 0o600)?;
        loop {
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
                Ok(()) => {
                    return Ok(FileGuard {
                        id: id.clone(),
                        _file: file,
                    });
                }
                // 待っている間にシグナルが届くと中断される。掴むのをやめる
                // 理由ではないので待ち直す。
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(std::io::Error::from(e).into()),
            }
        }
    }

    fn load(&self, id: &CredentialId) -> Result<V> {
        let path = self.path_of(id);
        let raw = fs::read_to_string(&path).map_err(|e| Error::Credential {
            id: id.to_string(),
            reason: format!("cannot read {}: {e}", path.display()),
        })?;
        serde_json::from_str(&raw).map_err(|e| Error::Credential {
            id: id.to_string(),
            reason: format!(
                "{} has an unexpected format: {e}. \
Log in again, or fix the file to match the stored format",
                path.display()
            ),
        })
    }

    fn reload(&self, guard: &FileGuard) -> Result<V> {
        self.load(&guard.id)
    }

    /// 書き換えは一時ファイル経由で行う。
    ///
    /// token の更新は「新しい値を受け取った時点で古い値が無効」なので、
    /// 書き込み途中で落ちると認証情報を失う。同じディレクトリに書いてから
    /// rename すれば、読み手からは切り替わる前か後のどちらかしか見えない。
    fn store(&self, guard: &FileGuard, value: &V) -> Result<()> {
        let path = self.path_of(&guard.id);
        // 一時ファイルの名前に書き手を混ぜる。共通の名前だと、同時に書いた
        // 2 者が同じファイルを切り詰め合い、混ざった JSON が rename される。
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));

        let json = serde_json::to_vec_pretty(value)?;
        {
            let mut f = fs::File::create(&tmp)?;
            restrict(&tmp, 0o600)?;
            f.write_all(&json)?;
            // rename する前にディスクへ落とす。ここを省くと、クラッシュ時に
            // 「rename は済んだが中身が空」というファイルが残りうる。
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 更新時刻を版として使う。書き換えは rename なので、中身が入れ替われば
    /// 必ず動く。
    fn version(&self, id: &CredentialId) -> Option<u64> {
        let modified = fs::metadata(self.path_of(id)).ok()?.modified().ok()?;
        let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(since_epoch.as_nanos() as u64)
    }

    fn list(&self) -> Result<Vec<CredentialId>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                ids.push(CredentialId::new(stem));
            }
        }
        ids.sort();
        Ok(ids)
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Entry {
        secret: String,
        priority: i32,
    }

    fn sample() -> Entry {
        Entry {
            secret: "s-1".into(),
            priority: 10,
        }
    }

    fn open(dir: &Path) -> FileStore<Entry> {
        FileStore::open(dir).unwrap()
    }

    fn put(store: &FileStore<Entry>, id: &CredentialId, value: &Entry) {
        let guard = store.lock(id).unwrap();
        store.store(&guard, value).unwrap();
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let id = CredentialId::new("someone");

        put(&store, &id, &sample());
        assert_eq!(store.load(&id).unwrap(), sample());
    }

    #[test]
    fn overwrite_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let id = CredentialId::new("someone");

        put(&store, &id, &sample());
        let guard = store.lock(&id).unwrap();
        let mut next = store.reload(&guard).unwrap();
        next.secret = "s-2".into();
        store.store(&guard, &next).unwrap();
        drop(guard);

        assert_eq!(store.load(&id).unwrap().secret, "s-2");
    }

    /// 権利は識別子ごと。別の識別子の権利は同時に取れ、書き込みは権利の
    /// 識別子へ向かう。
    #[test]
    fn a_guard_writes_to_its_own_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let a = store.lock(&CredentialId::new("a")).unwrap();
        let b = store.lock(&CredentialId::new("b")).unwrap();
        store.store(&b, &sample()).unwrap();

        assert!(store.load(&CredentialId::new("a")).is_err());
        assert_eq!(store.reload(&b).unwrap(), sample());
        assert!(store.reload(&a).is_err());
    }

    /// 書くと版が動く。無いものは版なし。
    #[test]
    fn version_follows_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let id = CredentialId::new("someone");
        assert_eq!(store.version(&id), None);

        put(&store, &id, &sample());
        assert!(store.version(&id).is_some());
    }

    /// 書き換え後に一時ファイルが残っていないか (残ると list に混ざる)。
    #[test]
    fn no_temp_file_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        put(&store, &CredentialId::new("a"), &sample());

        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with(".lock"))
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.json"]);
    }

    #[test]
    fn list_finds_json_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        put(&store, &CredentialId::new("b"), &sample());
        put(&store, &CredentialId::new("a"), &sample());
        fs::write(dir.path().join("notes.txt"), "無関係なファイル").unwrap();

        let ids = store.list().unwrap();
        assert_eq!(
            ids.iter().map(CredentialId::as_str).collect::<Vec<_>>(),
            vec!["a", "b"],
            "picks up only json, sorted by name"
        );
    }

    #[test]
    fn list_on_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open(dir.path()).list().unwrap().is_empty());
    }

    #[test]
    fn missing_file_reports_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let err = open(dir.path())
            .load(&CredentialId::new("nope"))
            .unwrap_err();
        assert!(
            err.to_string().contains("nope"),
            "identifies which one is missing: {err}"
        );
    }

    /// 壊れた JSON でも panic せず、どのファイルかを言う。
    #[test]
    fn broken_json_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        fs::write(dir.path().join("broken.json"), "{ not json").unwrap();

        let err = store.load(&CredentialId::new("broken")).unwrap_err();
        assert!(err.to_string().contains("broken.json"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn files_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        put(&store, &CredentialId::new("someone"), &sample());

        let mode = fs::metadata(dir.path().join("someone.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "not readable by anyone else");
    }
}
