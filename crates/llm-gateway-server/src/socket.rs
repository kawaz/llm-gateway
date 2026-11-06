//! 運用者の CLI と curl が読む Unix domain socket の口 (DR-0043 決定 7)。
//!
//! TCP と同じ Router を、呼び手の署名の層なしで出す ([`crate::Transport::Unix`])。
//! 相手を決めるのは socket ファイルの権限 (0600) と、繋いできたプロセスの uid
//! (自分と同じこと)。どちらも同じ uid のプロセスだけを通す線で、そのプロセスは
//! 設定も `[stats] dir` の控えも credential ファイルも読める者なので、信頼の境界は
//! 変わらない。

use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

/// 開いている運用系の口。落とすと socket ファイルも消す。
///
/// `axum::serve` にそのまま渡す。受けた接続のうち、相手の uid が自分と違うものは
/// HTTP を話す前に閉じる。
pub struct OperationsSocket {
    listener: UnixListener,
    path: PathBuf,
    /// bind した socket ファイル (device, inode)。消す時に、同じ path に別の者が
    /// 置き直した socket を消さないために比べる。
    file: (u64, u64),
    /// 通す uid (このプロセスの実効 uid)。
    owner: libc::uid_t,
}

impl OperationsSocket {
    /// `path` に開く。
    ///
    /// 親ディレクトリが無ければ 0700 で作る (既にあるディレクトリの権限は変えない:
    /// `[server] unix` は運用者が書く path で、`$HOME` や `/tmp` の下も指しうる)。
    /// 前回の残骸 (誰も待っていない socket) は消してから開く。socket でないファイル
    /// と、誰かが待っている socket は消さずに断る。
    pub fn bind(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        clear_stale(path)?;
        let listener = UnixListener::bind(path)?;
        // 制御口と同じく bind の後で閉じる。umask を触るとプロセス全体に効き、他の
        // スレッドが作るファイルまで巻き込む。閉じるまでの間に繋いだ相手も、
        // 受ける時の uid の検査で落ちる。
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            file: (meta.dev(), meta.ino()),
            // SAFETY: geteuid は引数を取らず、失敗しない。
            owner: unsafe { libc::geteuid() },
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OperationsSocket {
    fn drop(&mut self) {
        let ours = std::fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| (meta.dev(), meta.ino()) == self.file);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl axum::serve::Listener for OperationsSocket {
    type Io = UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            // 受け損ないの扱い (待って続ける) は axum の UnixListener の実装に任せる。
            let (stream, addr) = axum::serve::Listener::accept(&mut self.listener).await;
            let peer = stream.peer_cred().map(|cred| cred.uid());
            match admit(peer, self.owner) {
                Ok(()) => return (stream, addr),
                Err(reason) => {
                    tracing::warn!(socket = %self.path.display(), reason, "refused a connection to the operations socket");
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// 繋いできた相手を通すか。通すのは uid が `owner` と同じ相手だけで、uid を
/// 読めなかった相手も通さない。断る時は log に書く理由を返す。
fn admit(peer: io::Result<libc::uid_t>, owner: libc::uid_t) -> Result<(), String> {
    match peer {
        Ok(uid) if uid == owner => Ok(()),
        Ok(uid) => Err(format!("peer uid {uid} is not the unit's uid {owner}")),
        Err(e) => Err(format!("could not read the peer's uid: {e}")),
    }
}

/// `path` に残っている socket を片付ける。
///
/// 片付けるのは誰も待っていない socket だけ。待っている者がいる socket を消すと、
/// その者は待ち続けたまま誰にも繋がれなくなる (同じ path を書いた 2 台の取り合い)。
/// socket でないファイルは、path の書き間違いで設定ファイル等を消さないよう断る。
fn clear_stale(path: &Path) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 通すのは自分と同じ uid だけ。別の uid と、uid を読めなかった相手は断る。
    ///
    /// 別の uid から実際に繋ぐ試験は root が要るので書けない。検査の関数だけを確かめる。
    #[test]
    fn only_the_same_uid_is_admitted() {
        assert_eq!(admit(Ok(501), 501), Ok(()));

        let other = admit(Ok(502), 501).unwrap_err();
        assert!(other.contains("502") && other.contains("501"), "{other}");

        let unreadable = admit(Err(io::Error::other("ENOTCONN")), 501).unwrap_err();
        assert!(unreadable.contains("ENOTCONN"), "{unreadable}");
    }

    /// 開いた socket は 0600 で、親ディレクトリが無ければ 0700 で作る。落とすと消える。
    #[tokio::test]
    async fn the_socket_is_private_and_removed_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("made").join("gw.sock");

        let socket = OperationsSocket::bind(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_socket()
        );

        drop(socket);
        assert!(!path.exists(), "the socket file is removed");
    }

    /// 既にある親ディレクトリの権限は変えない (運用者が `$HOME` の下を指しうる)。
    #[tokio::test]
    async fn an_existing_directory_keeps_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let _socket = OperationsSocket::bind(&dir.path().join("gw.sock")).unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    /// 前回落ちた台が残した socket (誰も待っていない) は片付けて開ける。
    #[tokio::test]
    async fn a_stale_socket_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gw.sock");
        // 待ち受けを閉じても、std の UnixListener はファイルを消さない (落ちた台の残骸)。
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());

        let socket = OperationsSocket::bind(&path).unwrap();
        tokio::net::UnixStream::connect(&path)
            .await
            .expect("the new socket listens");
        drop(socket);
    }

    /// 誰かが待っている socket は消さずに断る (同じ path を書いた 2 台の取り合い)。
    #[tokio::test]
    async fn a_socket_someone_listens_on_is_not_taken() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gw.sock");
        let first = OperationsSocket::bind(&path).unwrap();

        let e = OperationsSocket::bind(&path).err().expect("refused");
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse, "{e}");
        tokio::net::UnixStream::connect(&path)
            .await
            .expect("the first one still listens");
        drop(first);
    }

    /// socket でないファイルは、path の書き間違いを疑って消さない。
    #[tokio::test]
    async fn a_file_that_is_not_a_socket_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[server]\n").unwrap();

        let e = OperationsSocket::bind(&path).err().expect("refused");
        assert!(e.to_string().contains("not a socket"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[server]\n");
    }

    /// 落とす時に消すのは自分が開いた socket だけ。同じ path に置き直された別の
    /// socket は残す。
    #[tokio::test]
    async fn dropping_leaves_a_socket_someone_else_put_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gw.sock");
        let socket = OperationsSocket::bind(&path).unwrap();
        // 退けるだけで消さない (inode が生きている間は、置き直す socket に同じ番号が付かない)。
        std::fs::rename(&path, dir.path().join("moved.sock")).unwrap();
        let _other = std::os::unix::net::UnixListener::bind(&path).unwrap();

        drop(socket);
        assert!(path.exists(), "the other socket stays");
    }
}
