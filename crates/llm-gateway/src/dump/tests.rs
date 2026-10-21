use super::*;
use crate::config::DumpConfig;
use crate::egress::{BodyStream, Headers};
use bytes::Bytes;
use futures_util::{FutureExt as _, StreamExt as _};
use std::os::unix::fs::PermissionsExt as _;

fn now() -> i64 {
    crate::credential::time::now_unix_ms()
}

fn dumps(dir: &Path) -> Arc<Dumps> {
    Arc::new(Dumps::new(dir))
}

/// 試験の `[dump]`。fs の空き容量の判定に掛からないよう `fs_min_free` は 0。
fn config() -> DumpConfig {
    DumpConfig {
        fs_min_free: 0,
        ..DumpConfig::default()
    }
}

fn start(dumps: &Arc<Dumps>, ns: &str, max_bytes: Option<u64>) -> Started {
    start_with(dumps, ns, max_bytes, &config())
}

fn start_with(
    dumps: &Arc<Dumps>,
    ns: &str,
    max_bytes: Option<u64>,
    config: &DumpConfig,
) -> Started {
    dumps
        .start(
            &Start {
                unit: "u",
                ns,
                max_bytes,
                ..Start::default()
            },
            config,
            now(),
        )
        .unwrap()
}

fn head(ns: &str, seq: u64, at: i64) -> RequestHead {
    RequestHead {
        ns: ns.to_owned(),
        seq,
        received_at: at,
        method: "POST".to_owned(),
        path: "/ns-lab/v1/messages".to_owned(),
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("authorization".to_owned(), "Bearer secret".to_owned()),
            ("anthropic-beta".to_owned(), "a".to_owned()),
            ("anthropic-beta".to_owned(), "b".to_owned()),
        ],
    }
}

/// ヘッダと本文を受けたことにする (本文は 3ms 後に受け終える)。
fn take(dumps: &Dumps, ns: &str, seq: u64, body: &'static [u8]) -> Option<Arc<Capture>> {
    let at = now();
    let capture = dumps.capture_head(|| head(ns, seq, at))?;
    capture.body(RequestBody {
        body_end_at: at + 3,
        body: Bytes::from_static(body),
        error: None,
        stream: true,
        model: Some("opus".to_owned()),
    });
    Some(capture)
}

fn body(chunks: &[&'static [u8]]) -> BodyStream {
    futures_util::stream::iter(
        chunks
            .iter()
            .map(|c| Ok(Bytes::from_static(c)))
            .collect::<Vec<_>>(),
    )
    .boxed()
}

fn upstream_headers() -> Headers {
    Headers::new(vec![
        ("content-type".to_owned(), "text/event-stream".to_owned()),
        ("request-id".to_owned(), "req_1".to_owned()),
    ])
}

/// upstream が `chunks` を返したことにして、tee を通して読み切る。
fn answer(
    capture: &Arc<Capture>,
    attempt: u64,
    route: &str,
    status: u16,
    chunks: &[&'static [u8]],
) {
    let connected_at = capture.connect(attempt, route);
    let headers = upstream_headers();
    let mut rewrites = Map::new();
    rewrites.insert("model".to_owned(), json!("claude-opus-5-5"));
    let read: Vec<_> = capture
        .tee(
            Upstream {
                attempt,
                route,
                credential: Some("cred"),
                status,
                headers: &headers,
                connected_at,
                rewrites,
            },
            body(chunks),
        )
        .collect::<Vec<_>>()
        .now_or_never()
        .expect("an in-memory stream is ready");
    assert_eq!(read.len(), chunks.len(), "every chunk still flows");
}

/// 1 本を受けて、試行 1 の応答をクライアントへ返し切る。
fn exchange(
    dumps: &Dumps,
    ns: &str,
    seq: u64,
    request: &'static [u8],
    chunks: &[&'static [u8]],
) -> bool {
    let Some(capture) = take(dumps, ns, seq, request) else {
        return false;
    };
    answer(&capture, 1, "r", 200, chunks);
    capture.adopt(1);
    capture.headers_sent();
    let settling = Arc::clone(&capture).settling();
    settling.body_end(chunks.iter().map(|c| c.len() as u64).sum(), true);
    true
}

/// 系列を止めて、書き手が書き終えるのを待ってから `index.jsonl` を読む。
fn finished(dumps: &Dumps, started: &Started) -> Vec<Value> {
    dumps.stop("u", &started.dump_id, now()).unwrap();
    events(started)
}

fn events(started: &Started) -> Vec<Value> {
    std::fs::read_to_string(Path::new(&started.dump_dir).join("index.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn names(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["event"].as_str().unwrap().to_owned())
        .collect()
}

fn read(started: &Started, name: &str) -> Vec<u8> {
    std::fs::read(Path::new(&started.dump_dir).join(name)).unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// 1 本の流れが時刻点ごとのイベントになり、request は受けた形、response は
/// upstream の形で、メタ json と body に分かれて残る (reference `files`)。
#[test]
fn an_exchange_leaves_its_events_metadata_and_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = start(&dumps, "lab", None);
    assert!(exchange(
        &dumps,
        "lab",
        7,
        b"{\"a\":1}",
        &[b"event: x\n", b"data: {}\n\n"]
    ));

    let events = finished(&dumps, &started);
    assert_eq!(
        names(&events),
        [
            "start",
            "request_headers",
            "request_body_end",
            "upstream_connect",
            "upstream_headers",
            "upstream_body_end",
            "upstream_close",
            "response_headers_sent",
            "response_body_end",
            "transport_close",
            "stop",
        ]
    );
    assert_eq!(events[0]["dump_id"], started.dump_id.as_str());
    assert_eq!(events[1]["seq"], 7);
    assert_eq!(events[2]["duration_ms"], 3);
    assert_eq!(events[4]["attempt"], 1);
    assert_eq!(events[4]["stream"], true);
    assert_eq!(events[5]["bytes"], 19);
    assert_eq!(events[5]["truncated"], false);
    assert_eq!(events[7]["adopted"], 1);
    assert_eq!(events[10]["reason"], "stop");
    assert_eq!(events[10]["bytes_written"], 7 + 19);

    let stem = format!("{}-7", events[1]["ts"]);
    let request: Value =
        serde_json::from_slice(&read(&started, &format!("{stem}.request.json"))).unwrap();
    assert_eq!(request["body_file"], format!("{stem}.request.body"));
    assert_eq!(request["body_stored"], "raw");
    assert_eq!(
        request["headers"],
        json!([
            {"name": "content-type", "value": "application/json"},
            {"name": "anthropic-beta", "value": "a"},
            {"name": "anthropic-beta", "value": "b"},
        ]),
        "authorization is dropped, the rest kept in order"
    );
    assert_eq!(
        read(&started, &format!("{stem}.request.body")),
        b"{\"a\":1}"
    );
    let response: Value =
        serde_json::from_slice(&read(&started, &format!("{stem}.1.response.json"))).unwrap();
    assert_eq!(response["status"], 200);
    assert_eq!(response["route"], "r");
    assert_eq!(response["credential"], "cred");
    assert_eq!(
        response["request_rewrites"]["model"],
        json!({"received": "opus", "sent": "claude-opus-5-5"})
    );
    assert_eq!(
        read(&started, &format!("{stem}.1.response.body")),
        b"event: x\ndata: {}\n\n"
    );

    let series = Path::new(&started.dump_dir);
    for level in series.ancestors().take(4) {
        assert_eq!(mode(level), 0o700, "{}", level.display());
    }
    for entry in std::fs::read_dir(series).unwrap().flatten() {
        assert_eq!(mode(&entry.path()), 0o600, "{}", entry.path().display());
    }
}

/// イベントは止まるのを待たずに、1 秒以内に `index.jsonl` へ書かれる。応答が
/// 来ないまま切れた 1 本にも `request_headers` が残る。
#[test]
fn events_reach_the_index_within_a_second() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = start(&dumps, "lab", None);
    // ヘッダを受けただけで、本文はまだ来ていない。
    let capture = dumps.capture_head(|| head("lab", 1, now())).unwrap();
    drop(capture);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if names(&events(&started)).contains(&"request_headers".to_owned()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "not written within a second"
        );
        // 書き手は別スレッドで、書いたことを知らせる口を持たない。1 秒の約束を
        // 確かめる試験なので、その間だけ見に行く。
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(dumps.list("u", &[], now()).unwrap()[0].state, "active");
}

/// 動いている系列が無い間・別の ns の 1 本は何もしない。
#[test]
fn only_the_namespace_of_a_running_dump_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    assert!(take(&dumps, "lab", 1, b"{}").is_none());
    assert!(!dir.path().join("dumps").exists(), "nothing was made");
    start(&dumps, "lab", None);
    assert!(take(&dumps, "other", 2, b"{}").is_none());
    assert!(take(&dumps, "lab", 3, b"{}").is_some());
}

/// 試行ごとに別のファイルに残り、返した試行を `response_headers_sent` の
/// `adopted` が指す。gateway が自分で組んだ応答なら `null`。
#[test]
fn every_attempt_is_kept_and_the_adopted_one_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = start(&dumps, "lab", None);
    let capture = take(&dumps, "lab", 1, b"{}").unwrap();
    answer(&capture, 1, "a", 429, &[b"denied"]);
    answer(&capture, 2, "a", 503, &[b"down"]);
    capture.adopt(1);
    capture.headers_sent();
    let refused = take(&dumps, "lab", 2, b"{}").unwrap();
    refused.headers_sent();

    let events = finished(&dumps, &started);
    let sent: Vec<&Value> = events
        .iter()
        .filter(|e| e["event"] == "response_headers_sent")
        .collect();
    assert_eq!(sent[0]["adopted"], 1);
    assert_eq!(sent[1]["adopted"], Value::Null);
    let stem = format!("{}-1", events[1]["ts"]);
    assert_eq!(
        read(&started, &format!("{stem}.1.response.body")),
        b"denied"
    );
    assert_eq!(read(&started, &format!("{stem}.2.response.body")), b"down");
    // 2 本の request (2 バイトずつ) と、試行 2 つの本文。
    assert_eq!(events.last().unwrap()["bytes_written"], 2 + 6 + 4 + 2);
}

/// 書くと上限を超える request は書かずに止まる。response は収まる分まで書いて
/// `truncated: true` で閉じてから止まる。上限ちょうどに達した時点でも止まる。
#[test]
fn the_size_limit_stops_a_dump_at_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());

    let refused = start(&dumps, "a", Some(4));
    exchange(&dumps, "a", 1, b"12345", &[]);
    let events = events_after_stop(&dumps, &refused);
    assert_eq!(names(&events), ["start", "request_headers", "stop"]);
    assert_eq!(events[2]["reason"], "max_bytes");

    let cut = start(&dumps, "b", Some(10));
    assert!(exchange(
        &dumps,
        "b",
        2,
        b"1234",
        &[b"abc", b"defgh", b"ij"]
    ));
    let events = events_after_stop(&dumps, &cut);
    let end = events
        .iter()
        .find(|e| e["event"] == "upstream_body_end")
        .unwrap();
    assert_eq!(end["truncated"], true);
    assert_eq!(end["bytes"], 6);
    assert_eq!(events.last().unwrap()["reason"], "max_bytes");
    assert_eq!(names(&events).iter().filter(|n| *n == "stop").count(), 1);
    let stem = format!("{}-2", events[1]["ts"]);
    assert_eq!(read(&cut, &format!("{stem}.1.response.body")), b"abcdef");

    let exact = start(&dumps, "c", Some(2));
    assert!(take(&dumps, "c", 3, b"12").is_some());
    assert_eq!(
        events_after_stop(&dumps, &exact).last().unwrap()["reason"],
        "max_bytes"
    );
}

/// 止まった系列の `index.jsonl` (止めたのは書き込みの側なので、`stop` で待つ)。
fn events_after_stop(dumps: &Dumps, started: &Started) -> Vec<Value> {
    let listed = dumps.stop("u", &started.dump_id, now()).unwrap();
    assert_eq!(listed.state, "stopped");
    events(started)
}

/// 全体の実使用量の上限に達したら、書こうとした系列だけが `total_limit` で
/// 止まる。request の来ない系列は active のまま残る (reference `capacity`)。
#[test]
fn the_total_limit_stops_only_the_dump_that_writes() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let small = DumpConfig {
        max_total_bytes: 10,
        ..config()
    };
    let writing = start_with(&dumps, "a", None, &small);
    let idle = start_with(&dumps, "b", None, &small);

    assert!(exchange(&dumps, "a", 1, b"1234", &[b"abcdefgh"]));

    let listed = dumps.list("u", &[], now()).unwrap();
    let state = |id: &str| listed.iter().find(|l| l.dump_id == id).unwrap().clone();
    assert_eq!(
        state(&writing.dump_id).reason.as_deref(),
        Some("total_limit")
    );
    assert_eq!(state(&idle.dump_id).state, "active");
    // 止まった系列の分は全体から外れるので、残った系列はまた書ける。
    assert!(exchange(&dumps, "b", 2, b"12", &[]));
}

/// fs の空き容量が `fs_min_free` を下回る書き込みは `fs_min_free` で止まる。
#[test]
fn the_free_space_floor_stops_a_dump() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let full = DumpConfig {
        fs_min_free: u64::MAX,
        ..config()
    };
    let started = start_with(&dumps, "a", None, &full);
    exchange(&dumps, "a", 1, b"{}", &[]);
    assert_eq!(
        events_after_stop(&dumps, &started).last().unwrap()["reason"],
        "fs_min_free"
    );
}

/// 止めた系列には、流れている途中の応答も以後は書かない。
#[test]
fn nothing_is_written_after_a_stop() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = start(&dumps, "lab", None);
    let capture = take(&dumps, "lab", 1, b"{}").unwrap();
    let connected_at = capture.connect(1, "r");
    let headers = upstream_headers();
    let mut teed = capture.tee(
        Upstream {
            attempt: 1,
            route: "r",
            credential: None,
            status: 200,
            headers: &headers,
            connected_at,
            rewrites: Map::new(),
        },
        body(&[b"first", b"second"]),
    );
    teed.next().now_or_never().unwrap().unwrap().unwrap();
    let listed = dumps.stop("u", &started.dump_id, now()).unwrap();
    let rest = teed.next().now_or_never().unwrap().unwrap().unwrap();
    assert_eq!(&rest[..], b"second", "the client still gets everything");
    drop(teed);
    capture.headers_sent();

    let events = events(&started);
    assert_eq!(names(&events).last().unwrap(), "stop");
    assert!(!names(&events).contains(&"upstream_body_end".to_owned()));
    assert_eq!(listed.bytes_written, Some(2 + 5));
}

/// 時限は書く時点の壁時計でも守る。過ぎた系列には書かず `timeout` で止まる。
#[test]
fn a_request_after_the_timeout_is_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = dumps
        .start(
            &Start {
                unit: "u",
                ns: "lab",
                timeout_ms: Some(1_000),
                ..Start::default()
            },
            &config(),
            now() - 1_000,
        )
        .unwrap();
    assert!(take(&dumps, "lab", 1, b"{}").is_none());
    assert!(dumps.running(now()).is_empty());
    assert_eq!(
        events_after_stop(&dumps, &started).last().unwrap()["reason"],
        "timeout"
    );
}

/// 時限の timer は unit が持ち、`expires_at` で系列を止める。
#[tokio::test]
async fn the_timer_stops_a_dump() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    dumps
        .start(
            &Start {
                unit: "u",
                ns: "lab",
                timeout_ms: Some(20),
                ..Start::default()
            },
            &config(),
            now(),
        )
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(dumps.active.load(Ordering::Acquire), 0);
}

/// 断る start: ns の名前・時限・容量・store・labels・同時に動かせる数。
#[test]
fn a_start_that_cannot_be_honoured_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let kind = |start: Start<'_>, config: &DumpConfig| {
        dumps.start(&start, config, now()).unwrap_err().kind
    };
    let base = || Start {
        unit: "u",
        ns: "lab",
        ..Start::default()
    };
    for ns in ["..", ".hidden", "a/b", ""] {
        assert_eq!(
            kind(Start { ns, ..base() }, &config()),
            "invalid_namespace",
            "{ns}"
        );
    }
    let day = 24 * 3600 * 1000;
    assert_eq!(
        kind(
            Start {
                timeout_ms: Some(0),
                ..base()
            },
            &config()
        ),
        "invalid_duration"
    );
    assert_eq!(
        kind(
            Start {
                timeout_ms: Some(day + 1),
                ..base()
            },
            &config()
        ),
        "invalid_duration"
    );
    assert_eq!(
        kind(
            Start {
                max_bytes: Some(0),
                ..base()
            },
            &config()
        ),
        "bad_request"
    );
    assert_eq!(
        kind(
            Start {
                store: Some("decoded"),
                ..base()
            },
            &config()
        ),
        "bad_request"
    );
    for (key, value) in [
        ("-a", "b"),
        ("a", ""),
        ("a", "b c"),
        (&"k".repeat(64)[..], "v"),
    ] {
        let labels = BTreeMap::from([(key.to_owned(), value.to_owned())]);
        assert_eq!(
            kind(Start { labels, ..base() }, &config()),
            "bad_request",
            "{key}={value}"
        );
    }
    let one = DumpConfig {
        max_concurrency: 1,
        ..config()
    };
    dumps.start(&base(), &one, now()).unwrap();
    assert_eq!(kind(base(), &one), "too_many_dumps");
}

/// `start` の答えは適用されたパラメータと、並列度・容量のスナップショット。
#[test]
fn a_start_answers_what_it_applied() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = start(&dumps, "lab", None);
    assert_eq!(started.timeout_ms, 5 * 60 * 1000);
    assert_eq!(started.max_bytes, 200 * 1024 * 1024);
    assert_eq!(started.store, "raw");
    assert_eq!(started.expires_at, started.started_at + started.timeout_ms);
    assert_eq!(started.delete_after, started.started_at + 48 * 3600 * 1000);
    assert_eq!(
        started.concurrency,
        Concurrency {
            limit: 16,
            active: 1
        }
    );
    assert_eq!(started.storage.active_requested_bytes, started.max_bytes);
    assert!(started.storage.fs_free_bytes > 0);
    assert!(Path::new(&started.dump_dir).ends_with(format!("dumps/u/lab/{}", started.dump_id)));
}

/// 片付けは `dump_id` の時刻部 + `cleanup_ttl` (= `delete_after`) で決める。
/// UUIDv7 でない名前は消さない。空になった段も消す。
#[test]
fn sweeping_removes_dumps_past_their_delete_after() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let ttl = 48 * 3600 * 1000;
    let old = uuid_v7(now() - ttl - 1, &mut |b| b.fill(0));
    let fresh = uuid_v7(now() - ttl + 60_000, &mut |b| b.fill(0));
    let root = dir.path().join("dumps");
    for path in [
        root.join("other/lab").join(&old),
        root.join("other/lab").join(&fresh),
        root.join("gone/x").join(&old),
        root.join("other/lab/not-a-uuid"),
    ] {
        std::fs::create_dir_all(&path).unwrap();
    }

    assert_eq!(dumps.sweep(now(), ttl), 2);
    assert!(root.join("other/lab").join(&fresh).exists());
    assert!(root.join("other/lab/not-a-uuid").exists());
    assert!(!root.join("gone").exists());
}

/// `list` はメモリの系列と、置き場に残っているだけの系列 (restart 前) を並べる。
/// 後者の理由は `stop` の行から、無ければ `interrupted`。選択子で絞れる。
#[test]
fn listing_includes_left_over_dumps_and_filters_by_selector() {
    let dir = tempfile::tempdir().unwrap();
    let before = dumps(dir.path());
    let labelled = before
        .start(
            &Start {
                unit: "u",
                ns: "lab",
                labels: BTreeMap::from([("owner".to_owned(), "kawaz".to_owned())]),
                ..Start::default()
            },
            &config(),
            now(),
        )
        .unwrap();
    let ended = start(&before, "lab", None);
    before.stop("u", &ended.dump_id, now()).unwrap();
    // restart 前に止められないまま落ちた系列 (`stop` の行が無い)。
    let crashed = uuid_v7(now(), &mut |b| b.fill(7));
    let crashed_dir = dir.path().join("dumps/u/lab").join(&crashed);
    std::fs::create_dir_all(&crashed_dir).unwrap();
    std::fs::write(crashed_dir.join("x.request.body"), b"12345").unwrap();
    before.stop("u", &labelled.dump_id, now()).unwrap();

    let after = dumps(dir.path());
    let listed = after.list("u", &[], now()).unwrap();
    assert_eq!(listed.len(), 3);
    let find = |id: &str| listed.iter().find(|l| l.dump_id == id).unwrap();
    assert_eq!(find(&ended.dump_id).reason.as_deref(), Some("stop"));
    assert_eq!(find(&crashed).reason.as_deref(), Some("interrupted"));
    assert_eq!(find(&crashed).bytes_written, Some(5));
    assert_eq!(find(&labelled.dump_id).labels["owner"], "kawaz");
    assert_eq!(
        find(&labelled.dump_id).expires_at,
        Some(labelled.expires_at)
    );

    let ids = |selectors: &[&str]| -> Vec<String> {
        let selectors: Vec<String> = selectors.iter().map(|s| (*s).to_owned()).collect();
        after
            .list("u", &selectors, now())
            .unwrap()
            .into_iter()
            .map(|l| l.dump_id)
            .collect()
    };
    assert_eq!(ids(&["owner=kawaz"]), [labelled.dump_id.as_str()]);
    assert_eq!(ids(&["owner"]), [labelled.dump_id.as_str()]);
    assert_eq!(ids(&["owner!=kawaz"]).len(), 2);
    let running = start(&after, "lab", None);
    assert_eq!(ids(&["state=active"]), [running.dump_id.as_str()]);
    assert_eq!(
        after
            .list("u", &["a b".to_owned()], now())
            .unwrap_err()
            .kind,
        "bad_request"
    );
    // 別の unit の分は並べない。
    assert!(after.list("other", &[], now()).unwrap().is_empty());
}

/// `stop` は冪等で、止まっていた系列にはその元の理由を返す。restart 前の系列も
/// 止まっている系列として答え、知らない id は `unknown_dump`。
#[test]
fn stopping_is_idempotent_and_keeps_the_first_reason() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = start(&dumps, "a", Some(1));
    take(&dumps, "a", 1, b"12");
    let first = dumps.stop("u", &started.dump_id, now()).unwrap();
    let again = dumps.stop("u", &started.dump_id, now()).unwrap();
    assert_eq!(first.reason.as_deref(), Some("max_bytes"));
    assert_eq!(again, first);
    assert_eq!(
        dumps.stop("u", "nope", now()).unwrap_err().kind,
        "unknown_dump"
    );

    let other = Dumps::new(dir.path());
    assert_eq!(
        other
            .stop("u", &started.dump_id, now())
            .unwrap()
            .reason
            .as_deref(),
        Some("max_bytes")
    );
}

/// `prune` は指定が無ければ断り、動いている系列は `stop` で止めてから消す。
#[test]
fn pruning_stops_and_removes_what_it_points_at() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    assert_eq!(
        dumps.prune("u", &[], &[], now()).unwrap_err().kind,
        "bad_request"
    );
    let kept = start(&dumps, "lab", None);
    let running = dumps
        .start(
            &Start {
                unit: "u",
                ns: "lab",
                labels: BTreeMap::from([("owner".to_owned(), "kawaz".to_owned())]),
                ..Start::default()
            },
            &config(),
            now(),
        )
        .unwrap();

    let pruned = dumps
        .prune("u", &[], &["owner=kawaz".to_owned()], now())
        .unwrap();
    assert_eq!(pruned, [running.dump_id.as_str()]);
    assert!(!Path::new(&running.dump_dir).exists());
    assert!(Path::new(&kept.dump_dir).exists());
    assert_eq!(dumps.active.load(Ordering::Acquire), 1);

    let pruned = dumps
        .prune("u", std::slice::from_ref(&kept.dump_id), &[], now())
        .unwrap();
    assert_eq!(pruned, [kept.dump_id.as_str()]);
    assert!(dumps.list("u", &[], now()).unwrap().is_empty());
}

/// UUIDv7 は時刻を先頭に持ち、版と変種の bit が立つ。時刻部を読み戻せる。
#[test]
fn a_dump_id_is_a_uuid_v7_ordered_by_time() {
    let id = uuid_v7(0x0190_0000_0001, &mut |buf| buf.fill(0xff));
    assert_eq!(id, "01900000-0001-7fff-bfff-ffffffffffff");
    assert_eq!(uuid_v7_ms(&id), Some(0x0190_0000_0001));
    assert_eq!(uuid_v7_ms("not-a-uuid"), None);
    assert!(uuid_v7(1, &mut |b| b.fill(0xff)) < uuid_v7(2, &mut |b| b.fill(0)));
}

fn following(dumps: &Arc<Dumps>, embed: bool, persist: bool) -> (Started, Following) {
    dumps
        .start_following(
            &Start {
                unit: "u",
                ns: "lab",
                follow: Some(Follow { embed, persist }),
                ..Start::default()
            },
            &config(),
            now(),
        )
        .unwrap()
}

/// 届いている行を全部取る (止まった系列は `stop` の後で閉じる)。
fn drain_lines(lines: &mut Following) -> Vec<Value> {
    let mut got = Vec::new();
    while let Some(line) = lines.blocking_recv() {
        got.push(serde_json::from_str(&line).unwrap());
    }
    got
}

/// follow は `index.jsonl` の行をそのまま流し (最初は `start`)、止まったら
/// `stop` の行の後で閉じる。ファイルも書く。
#[test]
fn following_streams_the_index_lines_and_ends_with_stop() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let (started, mut lines) = following(&dumps, false, true);
    assert!(exchange(&dumps, "lab", 1, b"{}", &[b"x"]));
    dumps.stop("u", &started.dump_id, now()).unwrap();

    let got = drain_lines(&mut lines);
    assert_eq!(names(&got).first().map(String::as_str), Some("start"));
    assert_eq!(names(&got).last().map(String::as_str), Some("stop"));
    assert_eq!(
        names(&got),
        names(&events(&started)),
        "the same lines as the file"
    );
    assert!(
        !names(&got).contains(&"body".to_owned()),
        "no bodies without embed"
    );
}

/// `--embed bodies` はメタ json と本文 (base64) も流す。`--no-persist` は止まった
/// 時にディレクトリごと消す。
#[test]
fn embedding_streams_bodies_and_no_persist_removes_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let (started, mut lines) = following(&dumps, true, false);
    assert!(exchange(&dumps, "lab", 1, b"{\"q\":1}", &[b"answer"]));
    dumps.stop("u", &started.dump_id, now()).unwrap();

    let got = drain_lines(&mut lines);
    let bodies: Vec<String> = got
        .iter()
        .filter(|e| e["event"] == "body")
        .map(|e| {
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD
                .decode(e["data_base64"].as_str().unwrap())
                .unwrap();
            String::from_utf8(data).unwrap()
        })
        .collect();
    assert_eq!(bodies, ["{\"q\":1}", "answer"]);
    let files: Vec<&Value> = got.iter().filter(|e| e["event"] == "file").collect();
    assert_eq!(files.len(), 2, "request.json and response.json");
    assert_eq!(files[1]["content"]["status"], 200);
    assert!(!Path::new(&started.dump_dir).exists(), "nothing is kept");
}

/// 残さず流しもしない系列は始めない。follow を求めない `start` で follow は断る。
#[test]
fn a_follow_that_shows_nothing_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let nothing = Start {
        unit: "u",
        ns: "lab",
        follow: Some(Follow {
            embed: false,
            persist: false,
        }),
        ..Start::default()
    };
    assert_eq!(
        dumps
            .start_following(&nothing, &config(), now())
            .unwrap_err()
            .kind,
        "bad_request"
    );
    let streaming = Start {
        follow: Some(Follow {
            embed: false,
            persist: true,
        }),
        ..nothing
    };
    assert_eq!(
        dumps.start(&streaming, &config(), now()).unwrap_err().kind,
        "bad_request"
    );
}

/// 購読口を捨てる (接続が閉じた) と `disconnected`、読まずに溢れさせると
/// `lagged` で系列が止まる。
#[test]
fn a_follower_that_leaves_or_falls_behind_stops_the_dump() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());

    let (gone, lines) = following(&dumps, false, true);
    dumps.disconnected(&gone.dump_id, now());
    drop(lines);
    assert_eq!(
        dumps
            .stop("u", &gone.dump_id, now())
            .unwrap()
            .reason
            .as_deref(),
        Some("disconnected")
    );

    let (slow, _lines) = following(&dumps, false, true);
    let capture = take(&dumps, "lab", 1, b"{}").unwrap();
    // 書き手の列 (4096) には収まり、follow の列 (1024) は溢れる数の行を置く。
    // 書き手が流すうちに溢れて、自分で系列を止める。
    for _ in 0..2_000 {
        capture.headers_sent();
    }
    dumps.wait_for_writer(&slow.dump_id);
    assert_eq!(
        dumps
            .stop("u", &slow.dump_id, now())
            .unwrap()
            .reason
            .as_deref(),
        Some("lagged")
    );
}

/// 本文を受け取りきれなかった・読めなかった 1 本も、受けたまま残し、
/// `request_body_end` と `*.request.json` に理由を付ける (DR-0038 決定 9)。
#[test]
fn a_body_that_could_not_be_read_is_kept_as_received() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = start(&dumps, "lab", None);
    let at = now();
    let capture = dumps.capture_head(|| head("lab", 1, at)).unwrap();
    capture.body(RequestBody {
        body_end_at: at + 1,
        body: Bytes::from_static(b"{\"cut"),
        error: Some("truncated"),
        stream: false,
        model: None,
    });
    capture.headers_sent();

    let events = finished(&dumps, &started);
    let end = events
        .iter()
        .find(|e| e["event"] == "request_body_end")
        .unwrap();
    assert_eq!(end["error"], "truncated");
    let stem = format!("{at}-1");
    assert_eq!(read(&started, &format!("{stem}.request.body")), b"{\"cut");
    let meta: Value =
        serde_json::from_slice(&read(&started, &format!("{stem}.request.json"))).unwrap();
    assert_eq!(meta["error"], "truncated");
}

/// 同時に始めても `max_concurrency` を超えない (枠を先に取る)。
#[test]
fn concurrent_starts_never_exceed_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let limited = DumpConfig {
        max_concurrency: 3,
        ..config()
    };
    let barrier = Arc::new(std::sync::Barrier::new(12));
    let handles: Vec<_> = (0..12)
        .map(|_| {
            let (dumps, barrier, limited) =
                (Arc::clone(&dumps), Arc::clone(&barrier), limited.clone());
            std::thread::spawn(move || {
                barrier.wait();
                dumps
                    .start(
                        &Start {
                            unit: "u",
                            ns: "lab",
                            ..Start::default()
                        },
                        &limited,
                        now(),
                    )
                    .map_err(|r| format!("{}: {}", r.kind, r.message))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        3,
        "{results:?}"
    );
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|k| k.starts_with("too_many_dumps"))
    );
    assert_eq!(dumps.active.load(Ordering::Acquire), 3);
}

/// 書いている最中に止めても、全体の実使用量に止めた系列の分が残らない
/// (二重にも引かない)。
#[test]
fn stopping_while_writing_leaves_no_bytes_in_the_total() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    for round in 0..20 {
        let started = start(&dumps, "lab", None);
        let capture = take(&dumps, "lab", round, b"{}").unwrap();
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let capture = Arc::clone(&capture);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let connected_at = capture.connect(1, "r");
                        let headers = upstream_headers();
                        let _ = capture
                            .tee(
                                Upstream {
                                    attempt: 1,
                                    route: "r",
                                    credential: None,
                                    status: 200,
                                    headers: &headers,
                                    connected_at,
                                    rewrites: Map::new(),
                                },
                                body(&[b"0123456789"]),
                            )
                            .collect::<Vec<_>>()
                            .now_or_never();
                    }
                })
            })
            .collect();
        dumps.stop("u", &started.dump_id, now()).unwrap();
        for writer in writers {
            writer.join().unwrap();
        }
        assert_eq!(dumps.usage.total(), 0, "round {round}");
    }
}

/// 空き容量を測り直しても、書き手がまだ書いていない分は空きに戻らない。
#[test]
fn measuring_again_keeps_what_is_still_to_be_written() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let usage = &dumps.usage;
    lock(&usage.accounting).fs_pending = 1000;
    usage.refresh(now(), true);
    let measured = usage.fs_measured.load(Ordering::Acquire);
    assert_eq!(lock(&usage.accounting).fs_available, measured - 1000);
    usage.written(1000);
    usage.refresh(now(), true);
    assert_eq!(
        lock(&usage.accounting).fs_available,
        usage.fs_measured.load(Ordering::Acquire)
    );
}

/// 書いている最中に測り直しが重なっても、書き終えれば未確定は 0 に戻り、
/// 止めた系列の分は全体に残らない。
#[test]
fn measuring_while_writing_settles_back_to_nothing_pending() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let started = start(&dumps, "lab", None);
    let stop_measuring = Arc::new(AtomicBool::new(false));
    let measuring = {
        let (dumps, stop) = (Arc::clone(&dumps), Arc::clone(&stop_measuring));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                dumps.usage.refresh(now(), true);
            }
        })
    };
    let writers: Vec<_> = (0..4)
        .map(|n| {
            let dumps = Arc::clone(&dumps);
            std::thread::spawn(move || {
                for seq in 0..100 {
                    exchange(&dumps, "lab", n * 1000 + seq, b"{}", &[b"0123456789"]);
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    stop_measuring.store(true, Ordering::Release);
    measuring.join().unwrap();
    dumps.stop("u", &started.dump_id, now()).unwrap();

    let accounting = lock(&dumps.usage.accounting);
    assert_eq!(accounting.fs_pending, 0);
    assert_eq!(accounting.total, 0);
}

/// 片付けは系列ごとの `start` の行の `delete_after` で決め、呼んだ unit の
/// `cleanup_ttl` を兄弟の unit の系列に当てない。`start` の行が読めない系列だけ
/// 呼んだ unit の値で代わりにする。
#[test]
fn sweeping_honours_each_dumps_own_delete_after() {
    let dir = tempfile::tempdir().unwrap();
    let hour = 3600 * 1000;
    let long = DumpConfig {
        cleanup_ttl: std::time::Duration::from_secs(96 * 3600),
        max_timeout: std::time::Duration::from_secs(24 * 3600),
        ..config()
    };
    // 兄弟の unit が 50 時間前に始めた、96 時間残す系列。
    let sibling = dumps(dir.path());
    let kept = sibling
        .start(
            &Start {
                unit: "sibling",
                ns: "lab",
                timeout_ms: Some(hour),
                ..Start::default()
            },
            &long,
            now() - 50 * hour,
        )
        .unwrap();
    sibling.stop("sibling", &kept.dump_id, now()).unwrap();
    // `start` の行が無い、50 時間前の系列。
    let unreadable = dir
        .path()
        .join("dumps/sibling/lab")
        .join(uuid_v7(now() - 50 * hour, &mut |b| b.fill(1)));
    std::fs::create_dir_all(&unreadable).unwrap();

    let this = dumps(dir.path());
    assert_eq!(this.sweep(now(), 48 * hour), 1);
    assert!(Path::new(&kept.dump_dir).exists(), "its own 96 hours apply");
    assert!(!unreadable.exists());
}

/// 書き手の列が溢れて渡せなかった本文の分は、fs の見込み・全体・系列の量から
/// 全部戻り、他の系列はそのまま書ける (戻さないと、測り直すまで他の系列が
/// `fs_min_free` で止まる)。
#[test]
fn a_body_that_could_not_be_queued_is_taken_back_from_every_count() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let other = start(&dumps, "other", None);
    let (stalled, queue) = dumps.stalled("lab", &config(), now());
    // 渡す先の列を誰も取り出さないまま埋め、`request_headers` の 1 行分だけ空ける。
    while stalled.writer.try_send(writer::Message::Wake).is_ok() {}
    queue.try_recv().unwrap();
    // 空きは 20 バイトだけ。
    lock(&dumps.usage.accounting).fs_available = 20;

    assert!(take(&dumps, "lab", 1, b"0123456789").is_some());
    assert!(
        !stalled.active.load(Ordering::Acquire),
        "the body could not be queued, so the dump stopped with error"
    );
    {
        let accounting = lock(&dumps.usage.accounting);
        assert_eq!(accounting.fs_available, 20);
        assert_eq!(accounting.fs_pending, 0);
        assert_eq!(accounting.total, 0);
    }

    // 戻っていなければ、ここで空きが床に達して `fs_min_free` で止まる。
    assert!(exchange(&dumps, "other", 2, b"0123456789", &[]));
    let events = finished(&dumps, &other);
    assert_eq!(events.last().unwrap()["reason"], "stop");
}

/// `request_headers` は列に置けたが本文で溢れた系列: 本文の分を全部の計数から戻す。
#[test]
fn a_reservation_that_was_not_queued_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let dumps = dumps(dir.path());
    let (stalled, _queue) = dumps.stalled("lab", &config(), now());
    {
        let mut accounting = lock(&dumps.usage.accounting);
        accounting.fs_available = 100;
    }
    let grant = stalled.reserve(40, true);
    assert_eq!(grant.bytes, 40);
    stalled.cancel(40);
    let accounting = lock(&dumps.usage.accounting);
    assert_eq!(
        (
            accounting.fs_available,
            accounting.fs_pending,
            accounting.total
        ),
        (100, 0, 0)
    );
    assert_eq!(stalled.written(), 0);
}
