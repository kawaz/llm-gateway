#![cfg(unix)]

//! 同じ状態ディレクトリで `daemon supervise` を 2 つ起こした時と、落ちた監督者の後に
//! 起こした時 (DR-0028 決定 7)。

use gateway_core::test_support::SocketTempDir;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_llm-gateway")
}

struct Supervising(Child);

impl Drop for Supervising {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn command(state: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(binary());
    command.args(args).env("XDG_STATE_HOME", state);
    command
}

fn supervise(state: &Path) -> Supervising {
    Supervising(
        command(state, &["daemon", "supervise"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

/// 頼み口に繋がるようになるまで待つ (別プロセスなので、開いた合図を受ける口が無い)。
fn wait_until_listening(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "{} did not listen",
            socket.display()
        );
        std::thread::yield_now();
    }
}

/// 終わるまで待ち、終わり方と stderr を返す。断られる側はすぐ終わるはずで、待ち続ける
/// なら監督を始めてしまっている。
fn finished(supervising: &mut Supervising) -> (ExitStatus, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = supervising.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "it went on to supervise instead of being refused"
        );
        std::thread::yield_now();
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = supervising.0.stderr.take() {
        pipe.read_to_string(&mut stderr).unwrap();
    }
    (status, stderr)
}

fn status(state: &Path) -> Output {
    command(state, &["daemon", "status"]).output().unwrap()
}

fn signal(supervising: &Supervising, name: &str) {
    let sent = Command::new("/bin/kill")
        .args([&format!("-{name}"), &supervising.0.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
}

/// 2 つ目の `daemon supervise` は JSON で断られて非 0 で終わり、1 つ目は頼みに答え続ける。
/// 1 つ目が SIGKILL で落ちて残骸 (誰も待っていない socket) を置いていっても、次は上がれる。
#[test]
fn a_second_supervisor_is_refused_and_a_dead_ones_leftovers_do_not_block_the_next() {
    let root = SocketTempDir::new().unwrap();
    let state = root.path().join("state");
    let socket = state.join("llm-gateway/daemon/supervisor.sock");

    let mut first = supervise(&state);
    wait_until_listening(&socket);

    let (ended, stderr) = finished(&mut supervise(&state));
    assert!(!ended.success(), "{stderr}");
    let refusal = stderr
        .lines()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .unwrap_or_else(|| panic!("no JSON on stderr: {stderr}"));
    assert_eq!(
        refusal["error"]["kind"],
        serde_json::json!("supervisor_already_running"),
        "{refusal}"
    );
    let hint = refusal["error"]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("daemon status"), "{refusal}");

    let asked = status(&state);
    assert!(
        asked.status.success(),
        "the first one still answers: {}",
        String::from_utf8_lossy(&asked.stderr)
    );

    // 落ちた監督者は socket ファイルを残す。lock は OS が外す。
    signal(&first, "KILL");
    finished(&mut first);
    assert!(
        socket.exists(),
        "a supervisor killed by SIGKILL leaves its socket behind"
    );

    let mut next = supervise(&state);
    wait_until_listening(&socket);
    assert!(status(&state).status.success(), "the next one answers");

    // 行儀よく降りれば、自分の socket を消す。
    signal(&next, "TERM");
    let (ended, stderr) = finished(&mut next);
    assert!(ended.success(), "{ended}: {stderr}");
    assert!(!socket.exists(), "the socket is removed when it stops");
}
