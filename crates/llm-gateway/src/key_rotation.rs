//! ns 認証 `jwt` の自動の鍵を入れ替える仕事 (DR-0037 §3・§13)。
//!
//! 各 unit の daemon が自分で走らせる。次にやることの時刻は鍵の作成時刻と寿命から
//! 壁時計で計算し、その時刻が来た ns のファイルだけを読み書きする。兄弟の unit と
//! 同じファイルを共有しても、書き手の調停は [`auto::rotate`] の flock が受け持つ。

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gateway_core::ns::jwt::auto;
use tokio::sync::Notify;

use crate::config::{Config, NsAuth};
use crate::router::Router;

/// 1 つの ns の入れ替えの指示。設定から写した値だけを持ち、走行中の設定とは
/// 切り離す (読み直しで差し替わっても、この回の指示は変わらない)。
#[derive(Debug, Clone)]
pub struct Job {
    pub ns: String,
    pub path: PathBuf,
    pub every: i64,
    pub max_token_ttl: i64,
    pub reserved: BTreeSet<String>,
}

/// 設定の `jwt` の ns ごとの指示。
///
/// 新しく作る鍵に刻む token の寿命の上限は `min(key_rotation, max_ttl)`
/// (DR-0037 §11)。
pub fn jobs(config: &Config) -> Vec<Job> {
    config
        .namespaces
        .iter()
        .filter_map(|(name, ns)| match &ns.auth {
            NsAuth::Jwt(jwt) => Some(Job {
                ns: name.clone(),
                path: config.auto_keys_path(name),
                every: jwt.key_rotation_secs,
                max_token_ttl: jwt.key_rotation_secs.min(jwt.max_ttl_secs),
                reserved: jwt.ring.kids().into_iter().collect(),
            }),
            _ => None,
        })
        .collect()
}

/// 時刻が来た ns (`due` に無い ns を含む) だけを入れ替え、`due` を次の時刻で
/// 埋め直す。設定から消えた ns は `due` から落とす。`now` は壁時計 (Unix 秒)。
///
/// 書けなかった ns は cooltime 後に試し直す。
pub fn step(
    jobs: &[Job],
    due: &mut HashMap<String, i64>,
    now: i64,
    fill: &mut dyn FnMut(&mut [u8]),
) {
    due.retain(|ns, _| jobs.iter().any(|job| &job.ns == ns));
    for job in jobs {
        if due.get(&job.ns).is_some_and(|at| now < *at) {
            continue;
        }
        let rotation = auto::Rotation {
            ns: &job.ns,
            path: &job.path,
            every: job.every,
            max_token_ttl: job.max_token_ttl,
            reserved: &job.reserved,
        };
        let next_at = match auto::rotate(&rotation, now, fill) {
            Ok(outcome) => {
                if let Some(broken) = &outcome.broken {
                    tracing::warn!(ns = %job.ns, path = %job.path.display(), moved_to = %broken.moved_to.display(), error = %broken.error, "cannot read the automatic keys; moved the file aside and started over with a new key (tokens signed by the old keys are refused until the client fetches a new one)");
                }
                if let Some(kid) = &outcome.created {
                    tracing::info!(ns = %job.ns, kid = %kid, path = %job.path.display(), "created an automatic key");
                }
                if !outcome.incomplete.is_empty() {
                    tracing::warn!(ns = %job.ns, kids = %outcome.incomplete.join(", "), path = %job.path.display(), "these lines say auto but lack created / max_token_ttl; they are kept and never used for signing");
                }
                if !outcome.deleted.is_empty() {
                    tracing::info!(ns = %job.ns, kids = %outcome.deleted.join(", "), "deleted old automatic keys");
                }
                outcome.next_at
            }
            Err(error) => {
                tracing::warn!(ns = %job.ns, %error, "cannot rotate the automatic keys; trying again later");
                now.saturating_add(auto::COOLTIME_SECS)
            }
        };
        due.insert(job.ns.clone(), next_at);
    }
}

/// 次に起きるまでの長さ。`jwt` の ns が無ければ `None` (読み直しを待つだけ)。
///
/// Design rationale: 次の時刻まで 1 回で寝ず、cooltime を上限に区切る。tokio の
/// timer は単調時計で数え、macOS の単調時計はシステムのスリープ中に止まるので、
/// 何日も先へ 1 回で寝るとスリープした時間だけ入れ替えが遅れる。起きたら壁時計で
/// 比べ直し、まだなら [`step`] は何も読まずに戻る (ファイルを見に行くポーリング
/// ではない)。
pub fn wait(due: &HashMap<String, i64>, now: i64) -> Option<Duration> {
    let next = *due.values().min()?;
    let secs = next.saturating_sub(now).clamp(1, auto::COOLTIME_SECS);
    Some(Duration::from_secs(secs.unsigned_abs()))
}

/// 1 回の入れ替えの結果を次の時刻の表にする。仕事が落ちた (panic 等) 時は、全 ns を
/// cooltime 後に試し直す表にする (空の表にすると、読み直しまで二度と起きない)。
pub fn settle(
    finished: Result<HashMap<String, i64>, tokio::task::JoinError>,
    names: &[String],
    now: i64,
) -> HashMap<String, i64> {
    match finished {
        Ok(due) => due,
        Err(error) => {
            tracing::warn!(%error, "the automatic key rotation stopped unexpectedly; trying again later");
            let retry = now.saturating_add(auto::COOLTIME_SECS);
            names.iter().map(|ns| (ns.clone(), retry)).collect()
        }
    }
}

/// 入れ替えの仕事を回し続ける。`wake` で起こされたら (設定の読み直し)、全 ns を
/// その場で確かめ直す。
pub async fn run(router: Arc<Router>, wake: Arc<Notify>) {
    let mut due: HashMap<String, i64> = HashMap::new();
    loop {
        let jobs = jobs(router.active().config());
        let now = crate::credential::time::now_unix();
        let names: Vec<String> = jobs.iter().map(|job| job.ns.clone()).collect();
        let finished = tokio::task::spawn_blocking(move || {
            step(&jobs, &mut due, now, &mut |buf: &mut [u8]| rand::fill(buf));
            due
        })
        .await;
        due = settle(finished, &names, now);
        let sleeping = async {
            match wait(&due, crate::credential::time::now_unix()) {
                Some(duration) => tokio::time::sleep(duration).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = sleeping => {}
            () = wake.notified() => due.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_270_000;
    const DAY: i64 = 86_400;

    fn config(stats: &std::path::Path, ring: &std::path::Path, extra: &str) -> Config {
        let text = format!(
            "[stats]\ndir = \"{}\"\n[ns.lab]\nauth = \"jwt\"\nkeys_file = \"{}\"\nmax_ttl = \"400d\"\n{extra}\n[ns.open]\n",
            stats.display(),
            ring.display()
        );
        toml::from_str(&text).unwrap()
    }

    fn manual_ring(dir: &std::path::Path) -> PathBuf {
        let ring = dir.join("lab.jwks.jsonl");
        let key = gateway_core::ns::jwt::signing_key(&[7; 32]);
        std::fs::write(
            &ring,
            gateway_core::ns::jwt::private_jwk("by-hand", &key).to_string(),
        )
        .unwrap();
        ring
    }

    fn fill() -> impl FnMut(&mut [u8]) {
        let mut n = 0u8;
        move |buf: &mut [u8]| {
            n = n.wrapping_add(1);
            buf.fill(n);
        }
    }

    /// `jwt` の ns だけが対象。上限は周期と `max_ttl` の小さい方、手の kid は避ける。
    #[test]
    fn jobs_come_from_jwt_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let ring = manual_ring(dir.path());
        let jobs = jobs(&config(dir.path(), &ring, "key_rotation = \"2d\""));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].ns, "lab");
        assert_eq!(jobs[0].path, dir.path().join("jwt-keys/lab.jwks.jsonl"));
        assert_eq!(jobs[0].every, 2 * DAY);
        assert_eq!(jobs[0].max_token_ttl, 2 * DAY);
        assert!(jobs[0].reserved.contains("by-hand"));
        let short = config(dir.path(), &ring, "").namespaces["lab"].clone();
        let NsAuth::Jwt(mut jwt) = short.auth else {
            panic!()
        };
        jwt.max_ttl_secs = 3600;
        let mut c = config(dir.path(), &ring, "");
        c.namespaces.get_mut("lab").unwrap().auth = NsAuth::Jwt(jwt);
        assert_eq!(super::jobs(&c)[0].max_token_ttl, 3600, "max_ttl caps it");
    }

    /// 時刻が来ていない ns のファイルは読みも書きもしない。来たら入れ替える。
    #[test]
    fn only_due_namespaces_touch_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let ring = manual_ring(dir.path());
        let jobs = jobs(&config(dir.path(), &ring, ""));
        let path = &jobs[0].path;
        let mut due = HashMap::new();
        step(&jobs, &mut due, NOW, &mut fill());
        assert_eq!(due["lab"], NOW + 7 * DAY, "created on the first pass");
        let first = std::fs::read(path).unwrap();

        // 時刻の前は、ファイルが消えていても触らない (= 読みに行っていない)。
        std::fs::remove_file(path).unwrap();
        step(&jobs, &mut due, NOW + 7 * DAY - 1, &mut fill());
        assert!(!path.exists());
        std::fs::write(path, &first).unwrap();

        step(&jobs, &mut due, NOW + 7 * DAY, &mut fill());
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 2, "a second key at the period");
        assert_eq!(
            due["lab"],
            NOW + 14 * DAY,
            "the next is the next period (the old key goes later)"
        );
    }

    /// 書けない ns は cooltime 後。設定から消えた ns は忘れる。
    #[test]
    fn failures_retry_after_the_cooltime_and_removed_namespaces_are_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let ring = manual_ring(dir.path());
        let jobs = jobs(&config(dir.path(), &ring, ""));
        // 鍵のディレクトリの場所にファイルがあって、`.lock` を作れない。
        std::fs::write(jobs[0].path.parent().unwrap(), "in the way").unwrap();
        let mut due = HashMap::from([("gone".to_owned(), NOW)]);
        let logs = crate::test_log::logs_of(|| {
            step(&jobs, &mut due, NOW, &mut fill());
            async {}
        });
        assert_eq!(due.get("lab"), Some(&(NOW + auto::COOLTIME_SECS)));
        assert!(!due.contains_key("gone"));
        assert!(logs.contains("trying again later"), "{logs}");
    }

    /// 読めない鍵のファイルは退避して新しい鍵で作り直し、退避先を警告に出す。次の時刻は
    /// 新しい鍵の周期。
    #[test]
    fn a_broken_ring_is_moved_aside_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let ring = manual_ring(dir.path());
        let jobs = jobs(&config(dir.path(), &ring, ""));
        let path = &jobs[0].path;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "broken").unwrap();
        let mut due = HashMap::new();
        let logs = crate::test_log::logs_of(|| {
            step(&jobs, &mut due, NOW, &mut fill());
            async {}
        });
        let moved_to = path.with_file_name("lab.jwks.jsonl.broken.20261006T070000Z");
        assert_eq!(std::fs::read_to_string(&moved_to).unwrap(), "broken");
        let warnings: Vec<&str> = logs.lines().filter(|l| l.contains("WARN")).collect();
        assert_eq!(warnings.len(), 1, "{logs}");
        assert!(
            warnings[0].contains("moved the file aside")
                && warnings[0].contains(&path.display().to_string())
                && warnings[0].contains(&moved_to.display().to_string()),
            "{logs}"
        );
        assert!(logs.contains("created an automatic key"), "{logs}");
        assert_eq!(due["lab"], NOW + 7 * DAY);
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 1, "one new key");
        assert!(auto::newest(path).unwrap().is_some());
    }

    /// 仕事が落ちても、全 ns を cooltime 後に試し直す。
    #[tokio::test]
    async fn a_crashed_pass_retries_every_namespace_after_the_cooltime() {
        let crashed =
            tokio::task::spawn_blocking(|| -> HashMap<String, i64> { panic!("boom") }).await;
        let names = vec!["lab".to_owned(), "personal".to_owned()];
        let due = settle(crashed, &names, NOW);
        let retry = NOW + auto::COOLTIME_SECS;
        assert_eq!(
            due,
            HashMap::from([("lab".to_owned(), retry), ("personal".to_owned(), retry)])
        );
        assert_eq!(
            wait(&due, NOW),
            Some(Duration::from_secs(auto::COOLTIME_SECS as u64))
        );
    }

    /// 寝る長さは次の時刻までで、cooltime を超えない。過ぎていても 1 秒は空ける。
    #[test]
    fn the_wait_is_capped_by_the_cooltime() {
        let due = |at: i64| HashMap::from([("a".to_owned(), at)]);
        assert_eq!(wait(&HashMap::new(), NOW), None);
        assert_eq!(wait(&due(NOW + 90), NOW), Some(Duration::from_secs(90)));
        assert_eq!(
            wait(&due(NOW + 7 * DAY), NOW),
            Some(Duration::from_secs(auto::COOLTIME_SECS as u64))
        );
        assert_eq!(wait(&due(NOW - 5), NOW), Some(Duration::from_secs(1)));
        let two = HashMap::from([("a".to_owned(), NOW + 500), ("b".to_owned(), NOW + 20)]);
        assert_eq!(wait(&two, NOW), Some(Duration::from_secs(20)));
    }
}
