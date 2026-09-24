//! 固定の秘密 (API キー / bearer) の読み手 (DR-0030 §5)。
//!
//! 他所から預かったキーを、手で置いたファイル (または差し替えた置き場) から
//! 読む。更新しないので [`super::refreshing`] は通らず、[`Persistence`] の
//! `load` と `version` だけを使う。版が変わったら読み直す (DR-0010 / DR-0022)。
//!
//! 読めなければ失敗を返す (fail-closed、DR-0031 §2 (1))。推測した値や古い値で
//! 上流へ出さない。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize, Serializer};

use super::{CredentialId, Persistence, StoredCredential, TaggedPayload};
use crate::Result;

/// 固定の秘密の中身。
///
/// 値は表示に出さない ([`fmt::Debug`] は伏せる)。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretValue {
    pub value: String,
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(..)")
    }
}

/// 保存ファイルの payload。種別は 1 つ (`static`) で、載せ方は使う側
/// (上流の設定) が決める。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum StaticSecret {
    Static(SecretValue),
}

impl StaticSecret {
    pub fn value(&self) -> &str {
        match self {
            Self::Static(v) => &v.value,
        }
    }
}

impl TaggedPayload for StaticSecret {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Static(_) => "static",
        }
    }

    fn serialize_body<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Static(v) => v.serialize(serializer),
        }
    }

    const OMITS_DEFAULT_TOP: bool = true;
}

/// ディスク上の固定の秘密。最小の形は `{"type":"static","payload":{"value":"…"}}`。
pub type StoredSecret = StoredCredential<(), StaticSecret>;

/// 控えの 1 件。読んだ時点の版を一緒に持つ。
struct Held {
    value: Arc<StoredSecret>,
    version: Option<u64>,
}

/// 固定の秘密を、版を見ながら読む窓口。
pub struct StaticSecretStore<P> {
    persistence: P,
    held: Mutex<HashMap<CredentialId, Held>>,
}

impl<P: Persistence<Value = StoredSecret>> StaticSecretStore<P> {
    pub fn new(persistence: P) -> Self {
        Self {
            persistence,
            held: Mutex::new(HashMap::new()),
        }
    }

    /// 今の中身。控えが今の版のままならそれを使い、違えば読み直す。
    ///
    /// 版を先に見る。読んだ後に見ると、読み終えてから書かれた中身を
    /// 「今の版」として覚え、その更新に気づけなくなる。
    pub fn get(&self, id: &CredentialId) -> Result<Arc<StoredSecret>> {
        let version = self.persistence.version(id);
        {
            let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(hit) = held.get(id)
                && hit.version == version
            {
                return Ok(Arc::clone(&hit.value));
            }
        }
        let value = Arc::new(self.persistence.load(id)?);
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id.clone(),
                Held {
                    value: Arc::clone(&value),
                    version,
                },
            );
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::file::FileStore;

    const MINIMAL: &str = r#"{
  "type": "static",
  "payload": {
    "value": "k-1"
  }
}"#;

    fn put(dir: &std::path::Path, id: &str, text: &str) {
        std::fs::write(dir.join(format!("{id}.json")), text).unwrap();
    }

    #[test]
    fn the_minimal_file_is_read_and_rewritten_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "s", MINIMAL);
        let files = FileStore::<StoredSecret>::open(dir.path()).unwrap();
        let guard = files.lock(&CredentialId::new("s")).unwrap();
        let value = files.reload(&guard).unwrap();
        assert_eq!(value.payload.value(), "k-1");
        assert_eq!(value.priority, 0);
        assert!(!value.disabled);
        files.store(&guard, &value).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("s.json")).unwrap(),
            MINIMAL
        );
    }

    #[test]
    fn written_top_fields_are_kept() {
        let text = r#"{"type":"static","priority":2,"disabled":true,"payload":{"value":"k"}}"#;
        let got: StoredSecret = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&got).unwrap(), text);
    }

    #[test]
    fn a_newer_version_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "s", MINIMAL);
        let secrets = StaticSecretStore::new(FileStore::<StoredSecret>::open(dir.path()).unwrap());
        let id = CredentialId::new("s");
        assert_eq!(secrets.get(&id).unwrap().payload.value(), "k-1");

        // 書き換えを版の違いとして見せる (同じ時刻の粒度に収まらないよう待たずに、
        // 更新時刻を明示して進める)。
        put(dir.path(), "s", &MINIMAL.replace("k-1", "k-2"));
        let file = std::fs::File::options()
            .write(true)
            .open(dir.path().join("s.json"))
            .unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(secrets.get(&id).unwrap().payload.value(), "k-2");
    }

    #[test]
    fn a_missing_secret_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = StaticSecretStore::new(FileStore::<StoredSecret>::open(dir.path()).unwrap());
        assert!(secrets.get(&CredentialId::new("none")).is_err());
    }

    #[test]
    fn the_value_is_not_printed() {
        let v = SecretValue {
            value: "k-1".into(),
        };
        assert!(!format!("{v:?}").contains("k-1"));
    }
}
