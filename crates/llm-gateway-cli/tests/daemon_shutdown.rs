#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn start(root: &Path, port: u16) -> Daemon {
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:{port}\"\nbinary_path = {:?}\n\n[store]\ntype = \"file\"\ndir = {:?}\n\n[stats]\ndir = {:?}\n",
            binary(),
            root.join("credentials"),
            root.join("stats"),
        ),
    )
    .unwrap();

    let added = Command::new(binary())
        .args(["daemon", "add", "--name", "probe"])
        .arg(&config)
        .env("XDG_STATE_HOME", &state)
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    Daemon(
        Command::new(binary())
            .args(["daemon", "run", "probe"])
            .env("XDG_STATE_HOME", state)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn subscribe(port: u16, endpoint: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() < deadline => {
                std::thread::yield_now();
                let _ = error;
            }
            Err(error) => panic!("server did not listen: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {endpoint} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    let mut byte = [0];
    while !response.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        response.push(byte[0]);
    }
    assert!(response.starts_with(b"HTTP/1.1 200"), "{response:?}");
    stream.set_read_timeout(None).unwrap();
    stream
}

fn terminate(mut daemon: Daemon) {
    let sent = Command::new("/bin/kill")
        .args(["-TERM", &daemon.0.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());

    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            assert!(status.success(), "daemon exited with {status}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not exit before its supervisor's 10s grace"
        );
        std::thread::yield_now();
    }
}

/// events は次の通知を無期限に待つ SSE だが、SIGTERM 後は子の 5 秒猶予内で切断し、
/// 監督者が 10 秒後に SIGKILL する前に daemon run 自身が exit 0 する。
#[test]
fn sigterm_stops_daemon_with_an_events_subscription() {
    let root = tempfile::TempDir::new().unwrap();
    let port = unused_port();
    let child = start(root.path(), port);
    let _subscription = subscribe(port, "/llm-gateway/events");

    terminate(child);
}

/// tap も次の交換を無期限に待つため、events と同じ終了上限で強制切断する。
#[test]
fn sigterm_stops_daemon_with_a_tap_subscription() {
    let root = tempfile::TempDir::new().unwrap();
    let port = unused_port();
    let child = start(root.path(), port);
    let _subscription = subscribe(port, "/llm-gateway/tap");

    terminate(child);
}
