#![cfg(unix)]

//! `daemon reload` を監督者 + 台 2 つで通す (DR-0032 段 4)。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_llm-gateway")
}

fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 監督者。落とす時は SIGTERM で、抱えた台も畳ませる。
struct Supervisor(Child);

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let _ = self.0.wait();
    }
}

/// 設定の本文。`extra` を末尾に足す。
fn config(root: &Path, port: u16, extra: &str) -> String {
    format!(
        r#"[server]
listen = "127.0.0.1:{port}"
binary_path = {binary:?}

[store]
type = "file"
dir = {store:?}

[stats]
dir = {stats:?}

[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:1"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
{extra}"#,
        binary = binary(),
        store = root.join("credentials"),
        stats = root.join("stats"),
    )
}

/// モデル `n` を足す変更。
const ADDS_N: &str = r#"
[routes.b]
provider = "anthropic"
url = "http://127.0.0.1:1"
models = ["n"]

[[ns.default.routing]]
models = ["n"]
routes = ["b"]
"#;

fn cli(state: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", state)
        .output()
        .unwrap()
}

/// モデルの一覧が `n` を含むか。繋がるまで待つ (台が待ち受けを始めるまで)。
fn lists_n(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "nothing listens on {port}: {error}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    write!(
        stream,
        "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    response.contains("\"n\"")
}

/// 1 台が失敗しても残りの台は読み直され、全台の結果が並び、exit は非 0。
#[test]
fn reloading_everything_reports_every_unit_and_fails_if_one_failed() {
    let root = tempfile::TempDir::new().unwrap();
    // unix socket のパス長の上限 (macOS 104 バイト) に収めるため短く。
    let state: PathBuf = root.path().join("s");
    std::fs::create_dir_all(&state).unwrap();
    let (port_a, port_b) = (unused_port(), unused_port());
    let (a, b) = (root.path().join("a.toml"), root.path().join("b.toml"));
    std::fs::write(&a, config(root.path(), port_a, "")).unwrap();
    std::fs::write(&b, config(root.path(), port_b, "")).unwrap();
    for (name, path) in [("a", &a), ("b", &b)] {
        let added = cli(
            &state,
            &["daemon", "add", "--name", name, path.to_str().unwrap()],
        );
        assert!(
            added.status.success(),
            "{}",
            String::from_utf8_lossy(&added.stderr)
        );
    }

    let _supervisor = Supervisor(
        Command::new(binary())
            .args(["daemon", "supervise"])
            .env("XDG_STATE_HOME", &state)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(!lists_n(port_a));
    assert!(!lists_n(port_b));

    // a は通る変更、b は変えられない欄 (listen) も変える。
    std::fs::write(&a, config(root.path(), port_a, ADDS_N)).unwrap();
    std::fs::write(&b, config(root.path(), unused_port(), ADDS_N)).unwrap();

    let reloaded = cli(&state, &["daemon", "reload", "--all"]);
    assert!(!reloaded.status.success(), "one unit failed");
    let said: serde_json::Value = serde_json::from_slice(&reloaded.stderr).unwrap_or_else(|e| {
        panic!(
            "{e}: {}{}",
            String::from_utf8_lossy(&reloaded.stdout),
            String::from_utf8_lossy(&reloaded.stderr)
        )
    });
    let units = said["units"].as_array().unwrap();
    assert_eq!(units.len(), 2, "{said}");
    assert_eq!(units[0]["unit"], "a");
    assert_eq!(units[0]["ok"], true, "{said}");
    assert_eq!(units[1]["unit"], "b");
    assert_eq!(units[1]["ok"], false, "{said}");
    assert_eq!(units[1]["error"]["kind"], "restart_required", "{said}");
    assert_eq!(units[1]["error"]["fields"][0], "[server] listen", "{said}");

    assert!(lists_n(port_a), "a runs the new one");
    assert!(!lists_n(port_b), "b keeps the old one");

    // 指した 1 台だけなら、その台の結果だけ。
    let one = cli(&state, &["daemon", "reload", "a"]);
    assert!(
        one.status.success(),
        "{}",
        String::from_utf8_lossy(&one.stderr)
    );
    let said: serde_json::Value = serde_json::from_slice(&one.stdout).unwrap();
    assert_eq!(said["units"].as_array().unwrap().len(), 1, "{said}");
}
