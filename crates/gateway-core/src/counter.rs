//! 合算可能なカウンタ (DR-0031 §2 (3) `CounterStore`)。
//!
//! 書き手ごとに自分の分だけを書き、読むときに全書き手の分を合わせる。書き手の
//! 間で同じ値を書き換えないので、排他は要らない。合わせ方は [`Mergeable`]
//! (可換・結合的で、`Default` が単位元)。
//!
//! 読めなかった書き手は [`Merged::missing`] に挙がる。欠けを許すかどうか
//! (fail-closed か best-effort か) は読み手が決める。

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::persist::{sanitize_writer, write_atomically};
use crate::stats::{Mergeable, Merged};

/// 書き手ごとに書いて、読むときに合わせる器。
pub trait CounterStore<C: Mergeable>: Send + Sync {
    /// 自分が書いた `bucket` の値。まだ書いていなければ `Default`。
    fn read_own(&self, bucket: &str) -> std::io::Result<C>;

    /// 自分の `bucket` の値を書く (自分の最新の累計で上書きする)。
    fn write_own(&self, bucket: &str, value: &C) -> std::io::Result<()>;

    /// 全書き手の `bucket` を合わせた値と、読めなかった書き手。
    fn read_merged(&self, bucket: &str) -> Merged<C>;
}

/// 書き手ごとのファイル (`<dir>/<bucket>.<writer>.json`) に置く実装。
///
/// `bucket` の `/` はディレクトリの区切りになる。書き込みは一時ファイル経由の
/// rename なので、読み手は書きかけを見ない。
pub struct FileCounters {
    dir: PathBuf,
    writer: String,
}

impl FileCounters {
    pub fn new(dir: impl Into<PathBuf>, writer: &str) -> Self {
        Self {
            dir: dir.into(),
            writer: sanitize_writer(writer),
        }
    }

    fn own_path(&self, bucket: &str) -> PathBuf {
        self.dir.join(format!("{bucket}.{}.json", self.writer))
    }
}

impl<C: Mergeable + Serialize + DeserializeOwned> CounterStore<C> for FileCounters
where
    C: Send + Sync,
{
    fn read_own(&self, bucket: &str) -> std::io::Result<C> {
        match read(&self.own_path(bucket)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(C::default()),
            other => other,
        }
    }

    fn write_own(&self, bucket: &str, value: &C) -> std::io::Result<()> {
        let path = self.own_path(bucket);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomically(&path, value)
    }

    fn read_merged(&self, bucket: &str) -> Merged<C> {
        let full = self.dir.join(bucket);
        let (parent, stem) = match (full.parent(), full.file_name().and_then(|n| n.to_str())) {
            (Some(parent), Some(stem)) => (parent.to_path_buf(), stem.to_owned()),
            _ => {
                return Merged {
                    value: C::default(),
                    missing: vec![self.writer.clone()],
                };
            }
        };
        let mut value = C::default();
        let mut missing = Vec::new();
        let entries = match std::fs::read_dir(&parent) {
            Ok(entries) => entries,
            // まだ誰も書いていない。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Merged { value, missing };
            }
            Err(_) => {
                return Merged {
                    value,
                    missing: vec!["*".to_owned()],
                };
            }
        };
        let prefix = format!("{stem}.");
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(writer) = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".json"))
            else {
                continue;
            };
            match read::<C>(&entry.path()) {
                Ok(part) => value.merge(&part),
                Err(_) => missing.push(writer.to_owned()),
            }
        }
        missing.sort();
        Merged { value, missing }
    }
}

fn read<C: DeserializeOwned>(path: &Path) -> std::io::Result<C> {
    let raw = std::fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(std::io::Error::other)
}

/// 数えた回数。合わせ方は加算。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RequestCount(pub u64);

impl Mergeable for RequestCount {
    fn merge(&mut self, other: &Self) {
        self.0 += other.0;
    }

    fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writers_are_merged_and_unreadable_ones_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        let a = FileCounters::new(dir.path(), "a");
        let b = FileCounters::new(dir.path(), "b");
        a.write_own("k/day-1", &RequestCount(3)).unwrap();
        b.write_own("k/day-1", &RequestCount(4)).unwrap();
        b.write_own("k/day-2", &RequestCount(9)).unwrap();
        let merged: Merged<RequestCount> = a.read_merged("k/day-1");
        assert_eq!((merged.value, merged.missing.len()), (RequestCount(7), 0));
        assert_eq!(
            CounterStore::<RequestCount>::read_own(&a, "k/day-1").unwrap(),
            RequestCount(3)
        );
        assert_eq!(
            CounterStore::<RequestCount>::read_own(&a, "k/none").unwrap(),
            RequestCount(0)
        );

        std::fs::write(dir.path().join("k/day-1.c.json"), "{ broken").unwrap();
        let merged: Merged<RequestCount> = a.read_merged("k/day-1");
        assert_eq!(merged.value, RequestCount(7));
        assert_eq!(merged.missing, vec!["c".to_owned()]);
    }

    #[test]
    fn nothing_written_yet_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let merged: Merged<RequestCount> =
            FileCounters::new(dir.path(), "a").read_merged("k/day-1");
        assert_eq!((merged.value, merged.missing.len()), (RequestCount(0), 0));
    }
}
