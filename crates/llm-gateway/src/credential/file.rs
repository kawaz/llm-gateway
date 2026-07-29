//! 平文ファイルへの保存。
//!
//! 1 認証情報 1 ファイルの JSON ([`StoredCredential`] の形)。ファイル名の
//! stem がそのまま識別子になる。
//!
//! 平文なので、同じ UID のプロセスなら誰でも読める。パーミッションは絞るが
//! それ以上の保護は無い。ここを埋めるのが cache-warden への移行。

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

use super::{CredentialId, Persistence, StoredCredential};

/// ディレクトリ 1 つを認証情報の置き場にする。
pub struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    /// 置き場を指定して開く。無ければ作る。
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        restrict(&dir, 0o700)?;
        Ok(Self { dir })
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
    /// 開いたまま抱えておくためだけに持つ。閉じるとロックが外れる。
    _file: fs::File,
}

impl Persistence for FileStore {
    type Guard = FileGuard;

    fn load(&self, id: &CredentialId) -> Result<StoredCredential> {
        let path = self.path_of(id);
        let raw = fs::read_to_string(&path).map_err(|e| Error::Credential {
            id: id.to_string(),
            reason: format!("{} を読めません: {e}", path.display()),
        })?;
        serde_json::from_str(&raw).map_err(|e| Error::Credential {
            id: id.to_string(),
            reason: format!(
                "{} の形式が想定と違います: {e}。\
`llm-gateway login` で取り直すか、認証情報を payload に入れた形に直してください",
                path.display()
            ),
        })
    }

    /// 書き換えは一時ファイル経由で行う。
    ///
    /// token の更新は「新しい値を受け取った時点で古い値が無効」なので、
    /// 書き込み途中で落ちると認証情報を失う。同じディレクトリに書いてから
    /// rename すれば、読み手からは切り替わる前か後のどちらかしか見えない。
    fn store(&self, id: &CredentialId, value: &StoredCredential) -> Result<()> {
        let path = self.path_of(id);
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
                Ok(()) => return Ok(FileGuard { _file: file }),
                // 待っている間にシグナルが届くと中断される。掴むのをやめる
                // 理由ではないので待ち直す。
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(std::io::Error::from(e).into()),
            }
        }
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
    use crate::credential::stored::{OauthTokens, Payload, StoredCredential};

    fn sample() -> StoredCredential {
        let mut c = StoredCredential::new(Payload::ClaudeOauth(OauthTokens {
            access_token: "at-1".into(),
            refresh_token: "rt-1".into(),
            expired: "2026-07-28T02:54:00+09:00".into(),
            email: "someone@example.com".into(),
            extra: Default::default(),
        }));
        c.priority = 10;
        c.last_refresh = "2026-07-27T18:54:00+09:00".into();
        c
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        let id = CredentialId::new("claude-someone");

        store.store(&id, &sample()).unwrap();
        let got = store.load(&id).unwrap();

        assert_eq!(got.payload.email(), "someone@example.com");
        assert_eq!(got.payload.secret(), "at-1");
        assert_eq!(got.priority, 10);
    }

    #[test]
    fn overwrite_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        let id = CredentialId::new("claude-someone");

        store.store(&id, &sample()).unwrap();
        let mut updated = sample();
        if let Some(tokens) = updated.payload.oauth_tokens_mut() {
            tokens.access_token = "at-2".into();
            tokens.refresh_token = "rt-2".into();
        }
        store.store(&id, &updated).unwrap();

        let got = store.load(&id).unwrap();
        assert_eq!(got.payload.secret(), "at-2");
        assert_eq!(got.payload.refresh_token(), Some("rt-2"));
    }

    /// 書き換え後に一時ファイルが残っていないか (残ると list に混ざる)。
    #[test]
    fn no_temp_file_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        store.store(&CredentialId::new("a"), &sample()).unwrap();

        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.json"]);
    }

    #[test]
    fn list_finds_json_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        store.store(&CredentialId::new("b"), &sample()).unwrap();
        store.store(&CredentialId::new("a"), &sample()).unwrap();
        fs::write(dir.path().join("notes.txt"), "無関係なファイル").unwrap();

        let ids = store.list().unwrap();
        assert_eq!(
            ids.iter().map(CredentialId::as_str).collect::<Vec<_>>(),
            vec!["a", "b"],
            "json 以外は拾わず、名前順に並ぶ"
        );
    }

    #[test]
    fn list_on_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn missing_file_reports_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        let err = store.load(&CredentialId::new("nope")).unwrap_err();
        assert!(
            err.to_string().contains("nope"),
            "どれが無いのか分かる: {err}"
        );
    }

    /// 壊れた JSON でも panic せず、どのファイルかを言う。
    #[test]
    fn broken_json_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        fs::write(dir.path().join("broken.json"), "{ not json").unwrap();

        let err = store.load(&CredentialId::new("broken")).unwrap_err();
        assert!(err.to_string().contains("broken.json"), "{err}");
    }

    /// 手で書いたファイルを読める (Bedrock は login できないので手で置く)。
    #[test]
    fn reads_a_hand_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        fs::write(
            dir.path().join("bedrock.json"),
            r#"{"type":"claude_bedrock","priority":5,
                "excluded_models":["claude-opus-*"],
                "payload":{"api_key":"ak","expired":"2026-08-02T10:08:18+09:00"}}"#,
        )
        .unwrap();

        let got = store.load(&CredentialId::new("bedrock")).unwrap();
        assert_eq!(got.payload.secret(), "ak");
        assert_eq!(got.excluded_models, vec!["claude-opus-*"]);
        assert!(!got.accepts_model("claude-opus-5"));
    }

    /// 旧形式 (平坦・cpa 互換) はどのファイルかを言って断る。
    ///
    /// 読めないままだと「なぜ動かないのか」が分からない。login し直せば
    /// 新しい形で書き直される。
    #[test]
    fn legacy_flat_file_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        fs::write(
            dir.path().join("claude-someone.json"),
            r#"{"type":"claude","email":"a@b.c","access_token":"at",
                "refresh_token":"rt","expired":"2026-07-28T02:54:00+09:00",
                "excluded-models":["claude-opus-*"]}"#,
        )
        .unwrap();

        let err = store
            .load(&CredentialId::new("claude-someone"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("claude-someone.json"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn files_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path()).unwrap();
        let id = CredentialId::new("claude-someone");
        store.store(&id, &sample()).unwrap();

        let mode = fs::metadata(dir.path().join("claude-someone.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "本人以外に読ませない");
    }
}
