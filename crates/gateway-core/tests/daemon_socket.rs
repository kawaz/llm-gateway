//! 頼み口を 1 つの持ち主に限る仕組み (`daemon::socket`、監督者の頼み口) の、手放した後と
//! 残骸の扱い。
//!
//! この試験は子プロセスを起こす試験 (監督者の試験) と同じバイナリに置かない。fork から
//! exec までの間、子は親の fd の写しを持つので、手放した lock や閉じた socket がその間だけ
//! 生きて見え、「手放した直後に次が握れる」「残骸を片付けられる」の確かめが揺れる。ここの
//! 試験はどれも子を起こさない (監督者は空の登録簿で動かす)。
#![cfg(unix)]

use std::io;
use std::path::Path;
use std::sync::Arc;

use gateway_core::daemon::protocol::{self, Request, Which};
use gateway_core::daemon::registry::Registry;
use gateway_core::daemon::socket::{clear_stale, hold, lock_path};
use gateway_core::daemon::supervisor::{NotStarted, Supervisor, UnitProbe};
use tempfile::TempDir;

/// 台を抱えないので、問い合わせ方は使われない。
const PROBE: UnitProbe = UnitProbe {
    listen_of: |_| None,
    health_path: "/unit/healthz",
    self_path: "/unit/self",
    version_path: "/unit/version",
};

fn supervisor(root: &Path, socket: &Path) -> Arc<Supervisor> {
    Arc::new(Supervisor::new(
        Registry::at(root.join("units")),
        root.join("logs"),
        socket.to_path_buf(),
        root.join("control"),
        PROBE,
    ))
}

/// 監督を始め、頼み口を開くところまで進めてから裏で走らせ続ける。開けなければ断られた
/// 理由を返す。
///
/// 1 度だけ進めれば頼み口は開いている (開くところまでは待つものが無い)。そこから先は
/// 頼みを待つだけなので、止める時は task を abort する (落とすと頼み口を手放す)。
async fn begin(supervisor: Arc<Supervisor>) -> Result<tokio::task::JoinHandle<()>, NotStarted> {
    let mut supervising = Box::pin(async move { supervisor.supervise().await });
    tokio::select! {
        biased;
        ended = &mut supervising => {
            return Err(ended.expect_err("it ended without being stopped"));
        }
        () = std::future::ready(()) => {}
    }
    Ok(tokio::spawn(async move {
        if let Err(e) = supervising.await {
            panic!("it stopped supervising: {e}");
        }
    }))
}

async fn stop(supervising: tokio::task::JoinHandle<()>) {
    supervising.abort();
    let ended = supervising.await;
    assert!(
        ended.is_err_and(|e| e.is_cancelled()),
        "it was still supervising"
    );
}

async fn answers(socket: &Path) -> bool {
    protocol::ask(socket, &Request::Status(Which::all()))
        .await
        .is_ok()
}

fn stale_socket(path: &Path) {
    // 待ち受けを閉じても、ファイルは消えない (std と同じ。落ちたプロセスの残骸)。
    drop(sockat::bind(path).unwrap());
    assert!(path.exists());
}

/// 握っている間、同じ path はもう握れない。手放せばまた握れる。
#[test]
fn one_holder_at_a_time() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.sock");

    let first = hold(&path).unwrap();
    let e = hold(&path).expect_err("refused while held");
    assert_eq!(e.kind(), io::ErrorKind::AddrInUse, "{e}");
    assert!(
        e.to_string()
            .contains(&lock_path(&path).display().to_string()),
        "{e}"
    );

    drop(first);
    let _again = hold(&path).unwrap();
}

/// 誰も待っていない socket は消し、誰かが待っている socket と socket でない
/// ファイルは残して断る。何も無ければ何もしない。
#[test]
fn only_a_socket_nobody_listens_on_is_cleared() {
    let dir = TempDir::new().unwrap();

    clear_stale(&dir.path().join("none.sock")).unwrap();

    let stale = dir.path().join("stale.sock");
    stale_socket(&stale);
    clear_stale(&stale).unwrap();
    assert!(!stale.exists());

    let live = dir.path().join("live.sock");
    let _listening = sockat::bind(&live).unwrap();
    let e = clear_stale(&live).expect_err("refused");
    assert_eq!(e.kind(), io::ErrorKind::AddrInUse, "{e}");
    assert!(live.exists());

    let file = dir.path().join("config.toml");
    std::fs::write(&file, "x").unwrap();
    let e = clear_stale(&file).expect_err("refused");
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
}

/// 落ちた監督者の残骸 (誰も待っていない socket と、持ち主の居ない lock) があっても、
/// 次の監督者は上がって頼みに答える。
#[tokio::test]
async fn a_supervisor_comes_up_over_what_a_dead_one_left() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("supervisor.sock");
    stale_socket(&socket);
    std::fs::write(lock_path(&socket), "").unwrap();

    let supervising = begin(supervisor(dir.path(), &socket))
        .await
        .expect("comes up over the leftovers");
    assert!(answers(&socket).await, "the new one answers");
    stop(supervising).await;
}

/// 2 つ目は断られ、終わった後も 1 つ目の頼み口は残って答え続ける。1 つ目が降りれば、
/// 頼み口は消え、次の監督者が上がれる。
#[tokio::test]
async fn a_second_supervisor_is_refused_and_the_first_keeps_its_socket() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("supervisor.sock");

    let first = begin(supervisor(dir.path(), &socket)).await.unwrap();
    let second = begin(supervisor(dir.path(), &socket))
        .await
        .expect_err("the second one is refused");
    assert!(
        matches!(second, NotStarted::AlreadyRunning(_)),
        "{second:?}"
    );
    assert!(answers(&socket).await, "the first one still answers");

    stop(first).await;
    assert!(!socket.exists(), "the first one removes its socket");
    let next = begin(supervisor(dir.path(), &socket))
        .await
        .expect("free again");
    assert!(answers(&socket).await);
    stop(next).await;
}

/// 降りる時に消すのは自分が開いた socket だけ。同じ path に別の者が置き直した socket は
/// 残す。
#[tokio::test]
async fn letting_go_removes_only_its_own_socket() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("supervisor.sock");

    let supervising = begin(supervisor(dir.path(), &socket)).await.unwrap();
    // 退けるだけで消さない (inode が生きている間は、置き直す socket に同じ番号が付かない)。
    std::fs::rename(&socket, dir.path().join("moved.sock")).unwrap();
    let _other = sockat::bind(&socket).unwrap();
    stop(supervising).await;
    assert!(socket.exists(), "a socket someone else put there stays");
}
