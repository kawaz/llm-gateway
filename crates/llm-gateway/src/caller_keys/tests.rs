use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use gateway_core::ns::jwt::{public_jwk, signing_key};
use serde_json::json;

use super::*;

const NOW: i64 = 1_791_270_000;

/// 1 回ずつ使う答えと、答えるまでの待ち。
type Queued = (Result<Vec<u8>, String>, Duration);

/// 答えを差し替えられる取得の口。呼ばれた `jwks_url` を数える。
#[derive(Default)]
pub(crate) struct FakeJwks {
    answers: Mutex<HashMap<Url, Result<Vec<u8>, String>>>,
    /// 1 回ずつ使う答えと、答えるまでの待ち。[`Self::answers`] より先に使う。
    queued: Mutex<HashMap<Url, VecDeque<Queued>>>,
    calls: Mutex<Vec<Url>>,
}

impl FakeJwks {
    /// `url` が次から返す答え。
    pub(crate) fn answer(&self, url: &Url, answer: Result<Vec<u8>, String>) {
        self.answers.lock().unwrap().insert(url.clone(), answer);
    }

    /// `url` が次から `(kid, 鍵の種)` の公開鍵の JWKS を返す。
    pub(crate) fn serve(&self, url: &Url, keys: &[(&str, u8)]) {
        self.answer(url, Ok(jwks_body(keys)));
    }

    /// `url` の次の 1 回は、`delay` 待ってから `keys` の JWKS を返す (積んだ順に使う)。
    pub(crate) fn serve_once_after(&self, url: &Url, keys: &[(&str, u8)], delay: Duration) {
        self.queued
            .lock()
            .unwrap()
            .entry(url.clone())
            .or_default()
            .push_back((Ok(jwks_body(keys)), delay));
    }

    /// `url` を取りに来た回数。
    pub(crate) fn calls(&self, url: &Url) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|called| *called == url)
            .count()
    }

    /// どこかを取りに来た回数。
    pub(crate) fn total_calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl FetchJwks for FakeJwks {
    fn fetch<'a>(&'a self, url: &'a Url) -> Fetching<'a> {
        self.calls.lock().unwrap().push(url.clone());
        let queued = self
            .queued
            .lock()
            .unwrap()
            .get_mut(url)
            .and_then(VecDeque::pop_front);
        let (answer, delay) = queued.unwrap_or_else(|| {
            let answer = self
                .answers
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err("nothing is served here".to_owned()));
            (answer, Duration::ZERO)
        });
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            answer
        })
    }
}

/// `(kid, 鍵の種)` の公開鍵を並べた JWKS の本文。鍵は `signing_key(&[種; 32])`。
pub(crate) fn jwks_body(keys: &[(&str, u8)]) -> Vec<u8> {
    let keys: Vec<_> = keys
        .iter()
        .map(|(kid, seed)| public_jwk(kid, &signing_key(&[*seed; 32]).verifying_key()))
        .collect();
    serde_json::to_vec(&json!({ "keys": keys })).unwrap()
}

fn url(name: &str) -> Url {
    Url::parse(&format!("https://{name}.example/jwks.json")).unwrap()
}

fn urls(names: &[&str]) -> BTreeSet<Url> {
    names.iter().map(|name| url(name)).collect()
}

/// 呼び手 `name` が `url(name)` を指す設定の形 ([`jwks_urls`] の形)。
fn callers(names: &[&str]) -> BTreeMap<Url, String> {
    names
        .iter()
        .map(|name| (url(name), (*name).to_owned()))
        .collect()
}

fn keys_with(fake: &Arc<FakeJwks>) -> CallerKeys {
    let fetcher: Arc<dyn FetchJwks> = Arc::clone(fake) as Arc<dyn FetchJwks>;
    CallerKeys::new(fetcher)
}

fn has(keys: &CallerKeys, name: &str, kid: &str, now: i64) -> Result<(), Missing> {
    keys.lookup(&url(name), kid, now).map(|_| ())
}

/// 全部取る時は設定の `jwks_url` を全部取り、取れた鍵を引ける。取れなかった
/// `jwks_url` は理由と一緒に返る。
#[tokio::test]
async fn syncing_everything_fetches_every_url() {
    let fake = Arc::new(FakeJwks::default());
    fake.serve(&url("a"), &[("a1", 1)]);
    let keys = keys_with(&fake);

    let failed = keys.sync(|| callers(&["a", "b"]), true, NOW).await;
    assert_eq!(fake.calls(&url("a")), 1);
    assert_eq!(fake.calls(&url("b")), 1);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, "b");
    assert_eq!(has(&keys, "a", "a1", NOW), Ok(()));
    assert_eq!(has(&keys, "a", "a2", NOW), Err(Missing::UnknownKid));
    assert_eq!(
        has(&keys, "b", "b1", NOW),
        Err(Missing::UnknownKid),
        "never fetched is an unknown kid, not stale"
    );
    assert_eq!(keys.urls(), urls(&["a", "b"]));
}

/// 束は最後の成功から 66 分まで信じ、過ぎたら `kid` を問わず stale。
#[tokio::test]
async fn keys_go_stale_66_minutes_after_the_last_success() {
    let fake = Arc::new(FakeJwks::default());
    fake.serve(&url("a"), &[("a1", 1)]);
    let keys = keys_with(&fake);
    keys.sync(|| callers(&["a"]), true, NOW).await;

    assert_eq!(has(&keys, "a", "a1", NOW + STALE_AFTER_SECS), Ok(()));
    assert_eq!(
        has(&keys, "a", "a1", NOW + STALE_AFTER_SECS + 1),
        Err(Missing::Stale)
    );
    assert_eq!(
        has(&keys, "a", "other", NOW + STALE_AFTER_SECS + 1),
        Err(Missing::Stale)
    );
    assert_eq!(STALE_AFTER_SECS, 3960);
}

/// 取れなかった回は前の束を保ち、期限は最後の成功から数える。
#[tokio::test]
async fn a_failed_fetch_keeps_the_previous_keys() {
    let fake = Arc::new(FakeJwks::default());
    fake.serve(&url("a"), &[("a1", 1)]);
    let keys = keys_with(&fake);
    keys.sync(|| callers(&["a"]), true, NOW).await;

    fake.answer(&url("a"), Err("down".to_owned()));
    let failed = keys.sync(|| callers(&["a"]), true, NOW + PERIOD_SECS).await;
    assert_eq!(failed, vec![("a".to_owned(), "down".to_owned())]);
    fake.answer(&url("a"), Ok(b"not json".to_vec()));
    let failed = keys
        .sync(|| callers(&["a"]), true, NOW + 2 * PERIOD_SECS)
        .await;
    assert_eq!(failed.len(), 1, "an unreadable JWKS is a failure too");

    assert_eq!(has(&keys, "a", "a1", NOW + 2 * PERIOD_SECS), Ok(()));
    assert_eq!(
        has(&keys, "a", "a1", NOW + STALE_AFTER_SECS + 1),
        Err(Missing::Stale),
        "the failures do not extend the trust"
    );
}

/// 取れたら丸ごと置き換え、載らなくなった `kid` は落ちる。
#[tokio::test]
async fn a_successful_fetch_replaces_the_whole_set() {
    let fake = Arc::new(FakeJwks::default());
    fake.serve(&url("a"), &[("old", 1), ("current", 2)]);
    let keys = keys_with(&fake);
    keys.sync(|| callers(&["a"]), true, NOW).await;
    assert_eq!(has(&keys, "a", "old", NOW), Ok(()));

    fake.serve(&url("a"), &[("current", 2), ("next", 3)]);
    keys.sync(|| callers(&["a"]), true, NOW + PERIOD_SECS).await;
    assert_eq!(
        has(&keys, "a", "old", NOW + PERIOD_SECS),
        Err(Missing::UnknownKid)
    );
    assert_eq!(has(&keys, "a", "current", NOW + PERIOD_SECS), Ok(()));
    assert_eq!(has(&keys, "a", "next", NOW + PERIOD_SECS), Ok(()));

    fake.serve(&url("a"), &[]);
    keys.sync(|| callers(&["a"]), true, NOW + 2 * PERIOD_SECS)
        .await;
    assert_eq!(
        has(&keys, "a", "current", NOW + 2 * PERIOD_SECS),
        Err(Missing::UnknownKid),
        "an empty JWKS drops every key"
    );
}

/// 新しいものだけ取る時は、持っていない `jwks_url` だけを取り、設定に無い
/// `jwks_url` の束を捨てる。
#[tokio::test]
async fn syncing_new_urls_fetches_only_those_and_drops_the_gone() {
    let fake = Arc::new(FakeJwks::default());
    fake.serve(&url("a"), &[("a1", 1)]);
    fake.serve(&url("b"), &[("b1", 2)]);
    fake.serve(&url("c"), &[("c1", 3)]);
    let keys = keys_with(&fake);
    keys.sync(|| callers(&["a", "b"]), true, NOW).await;

    keys.sync(|| callers(&["b", "c"]), false, NOW + 1).await;
    assert_eq!(fake.calls(&url("b")), 1, "b is kept as it is");
    assert_eq!(fake.calls(&url("c")), 1, "c is new");
    assert_eq!(keys.urls(), urls(&["b", "c"]));
    assert_eq!(has(&keys, "a", "a1", NOW + 1), Err(Missing::UnknownKid));
    assert_eq!(has(&keys, "c", "c1", NOW + 1), Ok(()));

    keys.sync(BTreeMap::new, true, NOW + 2).await;
    assert!(keys.urls().is_empty(), "no callers, no keys");
}

/// request (未知の `kid` か stale) での取り直しは `jwks_url` ごとに 30 秒に 1 回まで。
#[tokio::test]
async fn refetching_for_a_request_waits_the_minimum_interval() {
    let fake = Arc::new(FakeJwks::default());
    fake.serve(&url("a"), &[("a1", 1)]);
    fake.serve(&url("b"), &[("b1", 2)]);
    let keys = keys_with(&fake);
    keys.track(|| callers(&["a", "b"]));

    assert!(keys.refetch_for_request(&url("a"), NOW).await);
    assert_eq!(has(&keys, "a", "a1", NOW), Ok(()));
    assert!(
        !keys
            .refetch_for_request(&url("a"), NOW + REFETCH_INTERVAL_SECS - 1)
            .await
    );
    assert_eq!(fake.calls(&url("a")), 1);
    assert!(
        keys.refetch_for_request(&url("b"), NOW + 1).await,
        "the interval is per jwks_url"
    );
    assert!(
        keys.refetch_for_request(&url("a"), NOW + REFETCH_INTERVAL_SECS)
            .await
    );
    assert_eq!(fake.calls(&url("a")), 2);
}

/// 起動時・定期の取得は未知の `kid` の間隔を使わない (起動時の取得が失敗した直後の
/// 最初の request は取り直せる)。
#[tokio::test]
async fn scheduled_fetches_do_not_hold_back_the_refetch() {
    let fake = Arc::new(FakeJwks::default());
    let keys = keys_with(&fake);
    keys.sync(|| callers(&["a"]), true, NOW).await;
    fake.serve(&url("a"), &[("a1", 1)]);
    assert!(keys.refetch_for_request(&url("a"), NOW + 1).await);
    assert_eq!(has(&keys, "a", "a1", NOW + 1), Ok(()));
}

/// 遅れて終わった古い取得は、後から始まって先に入った束を上書きしない。同じ秒に
/// 始まった取得でも、始めた順で決まる。
#[tokio::test(start_paused = true)]
async fn an_older_fetch_finishing_later_does_not_overwrite_a_newer_one() {
    let fake = Arc::new(FakeJwks::default());
    let keys = keys_with(&fake);
    keys.track(|| callers(&["a"]));
    // 先に始まる定期の取得は遅く、後に始まる request の取り直しは速い。
    fake.serve_once_after(&url("a"), &[("old", 1)], Duration::from_secs(2));
    fake.serve_once_after(&url("a"), &[("new", 2)], Duration::from_secs(1));

    let a = url("a");
    let (failed, refetched) = tokio::join!(
        keys.sync(|| callers(&["a"]), true, NOW),
        keys.refetch_for_request(&a, NOW),
    );
    assert!(failed.is_empty());
    assert!(refetched);
    assert_eq!(fake.calls(&url("a")), 2);
    assert_eq!(has(&keys, "a", "new", NOW), Ok(()));
    assert_eq!(has(&keys, "a", "old", NOW), Err(Missing::UnknownKid));
}

/// 取っている間に設定から消えた `jwks_url` の結果は入れず、消えた `jwks_url` を戻さない。
/// 合わせる先は錠の中で読む設定なので、遅れて始まった定期の回も新しい設定に合わせる。
#[tokio::test(start_paused = true)]
async fn a_fetch_in_flight_for_a_removed_url_is_dropped() {
    let fake = Arc::new(FakeJwks::default());
    let keys = keys_with(&fake);
    let config = Mutex::new(callers(&["a", "b"]));
    let current = || config.lock().unwrap().clone();
    fake.serve_once_after(&url("a"), &[("a1", 1)], Duration::from_secs(2));
    fake.serve(&url("b"), &[("b1", 2)]);
    fake.serve(&url("c"), &[("c1", 3)]);

    let periodic = keys.sync(current, true, NOW);
    let reload = async {
        // 定期の回が a を取りに出た後に、a を消して c を足す読み直しが済む。
        tokio::time::sleep(Duration::from_secs(1)).await;
        *config.lock().unwrap() = callers(&["b", "c"]);
        keys.sync(current, false, NOW).await
    };
    let (periodic_failed, reload_failed) = tokio::join!(periodic, reload);
    assert!(periodic_failed.is_empty() && reload_failed.is_empty());
    assert_eq!(keys.urls(), urls(&["b", "c"]), "a is not put back");
    assert_eq!(has(&keys, "a", "a1", NOW), Err(Missing::UnknownKid));
    assert_eq!(has(&keys, "c", "c1", NOW), Ok(()));

    // 遅れて始まった定期の回は、その時の設定に合わせる。
    keys.sync(current, true, NOW + PERIOD_SECS).await;
    assert_eq!(keys.urls(), urls(&["b", "c"]));
    assert_eq!(fake.calls(&url("a")), 1);
}

/// 消えて同じ `jwks_url` で戻った呼び手に、消える前に始まった取得の結果は入れない
/// (戻った後の取得が失敗して、まだ何も入っていない時も)。
#[tokio::test(start_paused = true)]
async fn a_fetch_from_before_the_removal_does_not_fill_the_readded_url() {
    let fake = Arc::new(FakeJwks::default());
    let keys = keys_with(&fake);
    keys.track(|| callers(&["a"]));
    fake.serve_once_after(&url("a"), &[("before", 1)], Duration::from_secs(2));
    fake.answer(&url("a"), Err("down".to_owned()));

    let a = url("a");
    let stale_fetch = keys.refetch_for_request(&a, NOW);
    let reload_twice = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        keys.sync(BTreeMap::new, false, NOW).await;
        keys.sync(|| callers(&["a"]), false, NOW).await
    };
    let (refetched, failed) = tokio::join!(stale_fetch, reload_twice);
    assert!(refetched);
    assert_eq!(failed, vec![("a".to_owned(), "down".to_owned())]);
    assert_eq!(fake.calls(&url("a")), 2);
    assert_eq!(has(&keys, "a", "before", NOW), Err(Missing::UnknownKid));
}

/// 持っていない (設定から消えた) `jwks_url` は、request で取り直さず、載せもしない。
#[tokio::test]
async fn a_request_does_not_refetch_an_untracked_url() {
    let fake = Arc::new(FakeJwks::default());
    fake.serve(&url("a"), &[("a1", 1)]);
    let keys = keys_with(&fake);
    assert!(!keys.refetch_for_request(&url("a"), NOW).await);
    assert_eq!(fake.total_calls(), 0);
    assert!(keys.urls().is_empty());
}

/// 取りに出る側でも https でない `jwks_url` は断る。
#[tokio::test]
async fn only_https_is_fetched() {
    assert!(require_https(&url("a")).is_ok());
    let plain = Url::parse("http://a.example/jwks.json").unwrap();
    let err = require_https(&plain).unwrap_err();
    assert!(err.contains("https"), "{err}");
    let fetcher = HttpFetcher::new().unwrap();
    let err = fetcher.fetch(&plain).await.unwrap_err();
    assert!(err.contains("https"), "{err}");
}

/// 寝る長さは次の時刻までで、60 秒を超えない。過ぎていても 1 秒は空ける。
#[test]
fn the_wait_is_capped() {
    assert_eq!(wait(NOW + 20, NOW), Duration::from_secs(20));
    assert_eq!(wait(NOW + PERIOD_SECS, NOW), Duration::from_secs(60));
    assert_eq!(wait(NOW - 5, NOW), Duration::from_secs(1));
}

/// 試験の受け口。`/jwks` は `body` を返し、`/hop` は `/jwks` へ redirect する。
struct Served {
    base: Url,
    hits: Arc<AtomicUsize>,
}

async fn serve(body: Arc<Mutex<Vec<u8>>>, chunked: Arc<AtomicBool>) -> Served {
    use axum::routing::get;
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    let app = axum::Router::new()
        .route(
            "/jwks",
            get(move || {
                let body = body.lock().unwrap().clone();
                let chunked = chunked.load(Ordering::SeqCst);
                counted.fetch_add(1, Ordering::SeqCst);
                async move {
                    if chunked {
                        // 長さを名乗らずに流す。
                        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
                            body.chunks(1024).map(|c| Ok(c.to_vec())).collect();
                        axum::body::Body::from_stream(futures_util::stream::iter(chunks))
                    } else {
                        axum::body::Body::from(body)
                    }
                }
            }),
        )
        .route(
            "/hop",
            get(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [(axum::http::header::LOCATION, "/jwks")],
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Served {
        base: Url::parse(&format!("http://{addr}/")).unwrap(),
        hits,
    }
}

/// [`HttpFetcher`] から https の確かめだけを外した取得の口 (試験の受け口は http)。
struct PlainHttp(reqwest::Client);

impl FetchJwks for PlainHttp {
    fn fetch<'a>(&'a self, url: &'a Url) -> Fetching<'a> {
        Box::pin(get_bounded(&self.0, url))
    }
}

/// 64 KiB までは読み、超えたら (長さを名乗っても名乗らなくても) 失敗にする。
#[tokio::test]
async fn the_body_is_capped_at_64_kib() {
    let body = Arc::new(Mutex::new(vec![b' '; MAX_JWKS_BYTES]));
    let chunked = Arc::new(AtomicBool::new(false));
    let served = serve(Arc::clone(&body), Arc::clone(&chunked)).await;
    let client = jwks_client().unwrap();
    let jwks = served.base.join("jwks").unwrap();

    for streamed in [false, true] {
        chunked.store(streamed, Ordering::SeqCst);
        *body.lock().unwrap() = vec![b' '; MAX_JWKS_BYTES];
        assert_eq!(
            get_bounded(&client, &jwks).await.unwrap().len(),
            MAX_JWKS_BYTES,
            "chunked: {streamed}"
        );
        *body.lock().unwrap() = vec![b' '; MAX_JWKS_BYTES + 1];
        let err = get_bounded(&client, &jwks).await.unwrap_err();
        assert!(err.contains("exceeds"), "chunked: {streamed}: {err}");
    }
}

/// 64 KiB を超えた回は失敗で、前の束を保つ。
#[tokio::test]
async fn an_oversized_jwks_keeps_the_previous_keys() {
    let body = Arc::new(Mutex::new(jwks_body(&[("a1", 1)])));
    let served = serve(Arc::clone(&body), Arc::new(AtomicBool::new(false))).await;
    let jwks = served.base.join("jwks").unwrap();
    let keys = CallerKeys::new(Arc::new(PlainHttp(jwks_client().unwrap())));
    let set = BTreeMap::from([(jwks.clone(), "a".to_owned())]);

    assert!(keys.sync(|| set.clone(), true, NOW).await.is_empty());
    assert!(keys.lookup(&jwks, "a1", NOW).is_ok());

    let mut oversized = jwks_body(&[("a2", 2)]);
    oversized.resize(MAX_JWKS_BYTES + 1, b' ');
    *body.lock().unwrap() = oversized;
    let failed = keys.sync(|| set.clone(), true, NOW + PERIOD_SECS).await;
    assert_eq!(failed.len(), 1);
    assert!(failed[0].1.contains("exceeds"), "{}", failed[0].1);
    assert!(keys.lookup(&jwks, "a1", NOW + PERIOD_SECS).is_ok());
    assert_eq!(
        keys.lookup(&jwks, "a2", NOW + PERIOD_SECS),
        Err(Missing::UnknownKid)
    );
}

/// redirect は追わず、失敗にする (行き先は叩かない)。
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let body = Arc::new(Mutex::new(jwks_body(&[("a1", 1)])));
    let served = serve(body, Arc::new(AtomicBool::new(false))).await;
    let client = jwks_client().unwrap();

    let err = get_bounded(&client, &served.base.join("hop").unwrap())
        .await
        .unwrap_err();
    assert!(err.contains("302"), "{err}");
    assert_eq!(served.hits.load(Ordering::SeqCst), 0);
}

/// ログと失敗の理由に `jwks_url` (query や userinfo に秘密を含みうる) を出さず、呼び手の
/// 名前で出す。
#[test]
fn logs_name_the_caller_instead_of_the_url() {
    let secret = Url::parse("https://user:hunter2@a.example/jwks.json?token=hunter2").unwrap();
    let fake = Arc::new(FakeJwks::default());
    // 取り込めない JWK を混ぜて、取り込みで飛ばす警告も出させる。
    let body = json!({"keys": [{"kid": "bad", "kty": "RSA"}]});
    fake.answer(&secret, Ok(serde_json::to_vec(&body).unwrap()));
    let keys = keys_with(&fake);
    let set = BTreeMap::from([(secret.clone(), "notifier".to_owned())]);
    let logs = crate::test_log::logs_of(|| async {
        assert!(keys.sync(|| set.clone(), true, NOW).await.is_empty());
        fake.answer(&secret, Err("down".to_owned()));
        let failed = keys.sync(|| set.clone(), true, NOW + 1).await;
        assert_eq!(failed, vec![("notifier".to_owned(), "down".to_owned())]);
        assert!(keys.refetch_for_request(&secret, NOW + 2).await);
    });
    assert!(logs.contains("skipping a JWK"), "{logs}");
    assert!(logs.contains("[callers.notifier]"), "{logs}");
    assert_eq!(
        logs.matches("cannot fetch the keys of a caller").count(),
        2,
        "{logs}"
    );
    assert!(!logs.contains("hunter2"), "{logs}");
    assert!(!logs.contains("a.example"), "{logs}");
}

/// 繋がらなかった時の理由にも URL を載せない。
#[tokio::test]
async fn a_connection_failure_does_not_name_the_url() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let url = Url::parse(&format!("http://{addr}/jwks.json?token=hunter2")).unwrap();
    let err = get_bounded(&jwks_client().unwrap(), &url)
        .await
        .unwrap_err();
    assert!(!err.contains("hunter2"), "{err}");
}
