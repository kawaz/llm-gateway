//! cache に乗らなかった 1 本を、研究用に一定期間・一定量だけ取っておく。
//!
//! 乗らなかった request は控えない (入口) ・畳む (自送信) ので、本文はそこで
//! 消える。`cache_control` を持っているのに乗らない系列が実際にあり
//! (`docs/research/2026-09-14-claude-code-uncached-requests.md`)、後から
//! 「何が違ったのか」を見るには本文そのものが要る。
//!
//! 置き場は控えの下 (`<stats の置き場>/keepalive/uncached/`)。控えを拾う側は
//! `keepalive/` 直下の `.json` しか見ないので ([`super::store::Store::load_all`])、
//! ここに何を置いても系列の読み戻しには混ざらない。
//!
//! **ここに載るのも会話の本文そのもの**である。控えと同じ扱い (DR-0027 決定 4) —
//! 同意フラグも暗号化もマスキングも持たない。
//!
//! 溜め続けはしない。新しい順に [`Limits::keep`] 件、かつ [`Limits::days`] 日
//! までを残し、書くときと起動時に超えた分を捨てる。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::{Kept, Series, is_ours};
use crate::events;
use crate::metering::TokenUsage;
use crate::persist::write_atomically;

/// 1 日のミリ秒。保持期間を控えと同じ細かさ (ミリ秒) で測るために使う。
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// どこまで取っておくか (config の `[stats]`)。
///
/// どちらかが 0 なら**この仕掛けごと止まる** — 書かないし、既にあるものも
/// 触らない。止めた瞬間に手元の研究材料が消えるのは、止め方として強すぎる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// 新しい順にこの件数まで。
    pub keep: usize,
    /// この日数より古いものは捨てる。
    pub days: u32,
}

impl Limits {
    /// 取っておくかどうか。
    fn on(&self) -> bool {
        self.keep > 0 && self.days > 0
    }
}

/// 応答から読んだ、cache 判定の材料。
///
/// 「乗らなかった」と決めた根拠そのもの。後から見る側は、まずこれと本文を
/// 突き合わせる。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// 応答の usage から読んだ語。
    pub cache: events::Cache,
    /// upstream が返した状態。
    pub status: u16,
    /// 読めた usage。載らない応答では `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

/// どこで捨てた 1 本か。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dropped {
    /// 入口 — 実リクエストの応答が乗らなかったので、控えなかった。
    Entry,
    /// 自送信 — 送り直した 1 本が乗らなかったので、系列を畳んだ。
    Keepalive,
}

/// 取っておく 1 本。控えと同じ形に、判定の材料を添えたもの。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Uncached {
    pub dropped: Dropped,
    /// この 1 本を送った時刻 (Unix ミリ秒)。ファイル名にも入る。
    pub sent_at_ms: i64,
    #[serde(flatten)]
    pub evidence: Evidence,
    /// 本文・ヘッダ・model・route・ns・会話・系列。控えそのもの。
    pub kept: Kept,
}

impl Uncached {
    fn series(&self) -> Series {
        Series {
            session_id: self.kept.session_id.clone(),
            prefix: self.kept.prefix.clone(),
        }
    }
}

/// 乗らなかった 1 本の置き場。
pub struct Quarantine {
    dir: PathBuf,
    limits: Limits,
}

impl Quarantine {
    /// 控えの置き場の下、`uncached/` に 1 本ずつ置く。
    pub fn new(dir: impl AsRef<Path>, limits: Limits) -> Self {
        Self {
            dir: dir.as_ref().join("keepalive").join("uncached"),
            limits,
        }
    }

    fn path_of(&self, record: &Uncached) -> PathBuf {
        self.dir.join(format!(
            "{}.{}.json",
            record.series().stem(),
            record.sent_at_ms
        ))
    }

    /// 1 本置く。置けなかったことは転送や自送信を止める理由にならない。
    ///
    /// 置いた後に、はみ出した分をその場で捨てる — 起動まで待つと、書き続けた
    /// 分だけ置き場が膨らむ。
    pub fn put(&self, record: &Uncached) {
        if !self.limits.on() {
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(path = %self.dir.display(), %e, "cannot create the directory for uncached requests");
            return;
        }
        let path = self.path_of(record);
        if let Err(e) = write_atomically(&path, record) {
            tracing::warn!(path = %path.display(), %e, "cannot set aside the request that did not land on the cache");
            return;
        }
        tracing::debug!(
            session = %record.kept.session_id,
            prefix = %record.kept.prefix,
            cache = record.evidence.cache.as_str(),
            dropped = ?record.dropped,
            "set aside a request that did not land on the cache"
        );
        self.prune(record.sent_at_ms);
    }

    /// 新しい順に [`Limits::keep`] 件・[`Limits::days`] 日までに切り詰める。
    ///
    /// 時刻はファイル名から読む。中身を開かずに済むので、置き場が膨らんでいても
    /// 起動が重くならない。
    pub fn prune(&self, now_ms: i64) {
        if !self.limits.on() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut ours: Vec<(i64, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(sent_at_ms) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(sent_at_of)
            else {
                continue;
            };
            ours.push((sent_at_ms, path));
        }
        // 新しい順。同じ時刻に並んだものは名前で決める (どちらを残しても
        // 同じだが、決め方が実行ごとに変わると消える相手が揺れる)。
        ours.sort_by(|a, b| b.cmp(a));
        let oldest_kept_ms = now_ms - i64::from(self.limits.days) * DAY_MS;
        for (nth, (sent_at_ms, path)) in ours.iter().enumerate() {
            if nth < self.limits.keep && *sent_at_ms >= oldest_kept_ms {
                continue;
            }
            if let Err(e) = std::fs::remove_file(path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), %e, "cannot drop an uncached request past the limit");
            }
        }
    }
}

/// この名前は自分が置いたものか。そうなら、送った時刻。
///
/// 形は `<会話>.<系列>.<時刻>`。控えの名前 ([`Series::stem`]) に時刻を足した
/// ものなので、前半の判定はそのまま控えと同じ ([`is_ours`])。
fn sent_at_of(stem: &str) -> Option<i64> {
    let (series, sent_at_ms) = stem.rsplit_once('.')?;
    let sent_at_ms = sent_at_ms.parse().ok()?;
    is_ours(series).then_some(sent_at_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::RequestShape;

    fn limits() -> Limits {
        Limits { keep: 3, days: 7 }
    }

    fn record(session: &str, sent_at_ms: i64) -> Uncached {
        Uncached {
            dropped: Dropped::Entry,
            sent_at_ms,
            evidence: Evidence {
                cache: events::Cache::None,
                status: 200,
                usage: Some(TokenUsage::default()),
            },
            kept: Kept {
                session_id: session.to_owned(),
                prefix: "2cf24dba".to_owned(),
                ns: "default".to_owned(),
                model: "claude-opus-5".to_owned(),
                route: "a".to_owned(),
                body: serde_json::json!({"messages": [{"role": "user"}]}),
                headers: vec![("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned())],
                path: "/v1/messages".to_owned(),
                query: None,
                shape: RequestShape::Messages,
                fires_at_ms: sent_at_ms + 1,
                expires_at_ms: sent_at_ms + 2,
                horizon_end_ms: sent_at_ms + 3,
                since_ms: sent_at_ms,
                count: 0,
                cache_notice: None,
            },
        }
    }

    fn set_aside(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir.join("keepalive").join("uncached")) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// 置いた 1 本は、会話・系列・時刻の名前でそのまま読み戻せる。
    #[test]
    fn what_did_not_land_is_set_aside_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let quarantine = Quarantine::new(dir.path(), limits());

        let written = record("s-1", 1_800_000_000_000);
        quarantine.put(&written);

        assert_eq!(set_aside(dir.path()), ["s-1.2cf24dba.1800000000000.json"]);
        let raw = std::fs::read(
            dir.path()
                .join("keepalive/uncached/s-1.2cf24dba.1800000000000.json"),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Uncached>(&raw).unwrap(),
            written,
            "the body and the evidence come back together"
        );
    }

    /// 残るのは新しい順に `keep` 件。古いものから消える。
    #[test]
    fn only_the_newest_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let quarantine = Quarantine::new(dir.path(), limits());

        for nth in 0..5 {
            quarantine.put(&record("s-1", 1_800_000_000_000 + nth));
        }

        assert_eq!(
            set_aside(dir.path()),
            [
                "s-1.2cf24dba.1800000000002.json",
                "s-1.2cf24dba.1800000000003.json",
                "s-1.2cf24dba.1800000000004.json",
            ],
            "the three newest survive"
        );
    }

    /// 期間を過ぎたものは、件数に収まっていても消える。
    #[test]
    fn what_is_older_than_the_window_goes() {
        let dir = tempfile::tempdir().unwrap();
        let quarantine = Quarantine::new(dir.path(), limits());

        let now_ms = 1_800_000_000_000;
        quarantine.put(&record("s-old", now_ms - 8 * DAY_MS));
        quarantine.put(&record("s-new", now_ms));

        assert_eq!(
            set_aside(dir.path()),
            [format!("s-new.2cf24dba.{now_ms}.json")],
            "only what is inside the window is left"
        );
    }

    /// 起動時にも同じ切り詰めが効く (書き込みを挟まずに呼べる)。
    #[test]
    fn the_limits_also_apply_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let now_ms = 1_800_000_000_000;
        {
            let wide = Quarantine::new(dir.path(), Limits { keep: 50, days: 30 });
            for nth in 0..5 {
                wide.put(&record("s-1", now_ms + nth));
            }
            assert_eq!(set_aside(dir.path()).len(), 5);
        }

        Quarantine::new(dir.path(), limits()).prune(now_ms);
        assert_eq!(set_aside(dir.path()).len(), 3, "trimmed on the way up");
    }

    /// 0 なら何も置かない。既にあるものも触らない。
    #[test]
    fn nothing_is_set_aside_when_it_is_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let now_ms = 1_800_000_000_000;
        Quarantine::new(dir.path(), limits()).put(&record("s-1", now_ms));

        for off in [Limits { keep: 0, days: 7 }, Limits { keep: 50, days: 0 }] {
            let quarantine = Quarantine::new(dir.path(), off);
            quarantine.put(&record("s-2", now_ms + 1));
            quarantine.prune(now_ms + DAY_MS * 365);
            assert_eq!(
                set_aside(dir.path()),
                [format!("s-1.2cf24dba.{now_ms}.json")],
                "nothing was written, and what was there stayed"
            );
        }
    }

    /// 自分の名前で置いていないファイルは、数えも消しもしない。
    #[test]
    fn a_file_someone_else_left_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let quarantine = Quarantine::new(dir.path(), limits());
        let now_ms = 1_800_000_000_000;
        quarantine.put(&record("s-1", now_ms));
        let theirs = dir.path().join("keepalive/uncached/notes.json");
        std::fs::write(&theirs, b"{}").unwrap();

        for nth in 1..=4 {
            quarantine.put(&record("s-1", now_ms + nth));
        }

        assert!(theirs.exists(), "it is not ours to drop");
        let ours = set_aside(dir.path());
        assert_eq!(ours.len(), 4, "3 of ours plus the file we left alone");
        assert!(ours.contains(&"notes.json".to_owned()));
    }
}
