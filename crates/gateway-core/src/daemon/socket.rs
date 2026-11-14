//! 1 つの Unix socket の path を、同時に 1 つのプロセスにだけ開かせる部品。
//!
//! 監督者の頼み口 (DR-0028 決定 7) と台の運用系の口 (DR-0043 決定 7) が同じ流儀で使う:
//!
//! 1. 脇の `<path>.lock` を flock で握る ([`hold`])。握れなければ、同じ path を別の
//!    プロセスが開いているので断る (待たない。握っている側は開いている間ずっと離さない)
//! 2. 握れたら、前回落ちたプロセスが残した socket を片付ける ([`clear_stale`])
//! 3. bind する。落とす時は、自分が bind した socket (dev / inode が同じもの) だけを消す
//!
//! 「先に居る者が生きているか」を繋いで確かめるだけだと、確かめてから bind するまでの
//! 間に同時に来た 2 つが両方「居ない」と見て両方 bind する。flock は握った者が落ちれば
//! OS が外すので、残骸の lock が次の者を締め出すことも無い。

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// `path` の socket を開く権利を表す lock ファイル (`<path>.lock`)。
pub fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// `<path>.lock` を握る。握れなければ (別のプロセスがこの path を開いている) `AddrInUse`。
///
/// 返した `File` を持っている間握り続け、落とすと外れる。`.lock` は消さない
/// (消して作り直すと、握っている側と後から来た側が別のファイルを見て締め出しが破れる)。
pub fn hold(path: &Path) -> io::Result<File> {
    let lock_path = lock_path(path);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another process holds {} ({} is locked)",
                path.display(),
                lock_path.display()
            ),
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// `path` に残っている socket を片付ける。[`hold`] で握った後で呼ぶ。
///
/// 片付けるのは誰も待っていない socket だけ。lock を握れた以上この path を開いている
/// 仲間は居ないが、lock を知らない別のプロセス (lock を持たない古い版を含む) が待って
/// いることはありうる。その socket を消すと、その者は待ち続けたまま誰にも繋がれなく
/// なるので、`AddrInUse` で断る。
/// socket でないファイルは、path の書き間違いで設定ファイル等を消さないよう断る。
pub fn clear_stale(path: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not a socket; not replacing it",
                path.display()
            ),
        ));
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another process is listening on {}", path.display()),
        )),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
        Err(e) => Err(e),
    }
}

// 手放した直後に握り直す・残骸を片付ける、の試験は `tests/daemon_socket.rs` に置く
// (子プロセスを起こす試験と同じバイナリだと、fork から exec までの間に子が持つ fd の
// 写しで、手放した lock や閉じた socket がまだ生きて見えることがある)。
#[cfg(test)]
mod tests {
    use super::*;

    /// lock は socket の隣に、名前に `.lock` を足して置く。
    #[test]
    fn the_lock_sits_beside_the_socket() {
        assert_eq!(
            lock_path(Path::new("/state/daemon/supervisor.sock")),
            Path::new("/state/daemon/supervisor.sock.lock")
        );
    }

    /// 持ち主の居ない `.lock` (落ちたプロセスの残骸) は、次の者を締め出さない。
    #[test]
    fn a_lock_file_left_behind_does_not_lock_anyone_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sock");
        std::fs::write(lock_path(&path), "").unwrap();

        let _held = hold(&path).unwrap();
    }
}
