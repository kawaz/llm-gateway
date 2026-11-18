//! 運用者の CLI と curl が読む Unix domain socket の口 (DR-0043 決定 7)。
//!
//! TCP と同じ Router を、呼び手の署名の層なしで出す ([`crate::Transport::Unix`])。
//! 相手を決めるのは socket ファイルの権限 (0600) と、繋いできたプロセスの uid
//! (自分と同じこと)。どちらも同じ uid のプロセスだけを通す線で、そのプロセスは
//! 設定も `[stats] dir` の控えも credential ファイルも読める者なので、信頼の境界は
//! 変わらない。
//!
//! 開くのも繋ぐのも [`gateway_core::unix_socket`] (sockat) を通すので、path の長さは
//! `sun_path` に縛られない。CLI 側の読み手 [`request`] も同じ理由でここに置く
//! (reqwest の Unix socket は自前で connect し、長い path に届かない)。

use std::fs::File;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, Response, header};
use gateway_core::daemon::socket::{clear_stale, hold};
use hyper_util::rt::TokioIo;
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
    /// 脇の `<path>.lock` の flock。開いている間握り続け、同じ path を開こうとする
    /// 他の台を締め出す。外れるのは socket を消した後 (欄は `drop` の本体の後で落ちる)。
    _lock: File,
}

impl OperationsSocket {
    /// `path` に開く。
    ///
    /// 親ディレクトリが無ければ 0700 で作る (既にあるディレクトリの権限は変えない:
    /// `[server] unix` は運用者が書く path で、`$HOME` の下も指しうる)。親ディレクトリが
    /// 自分のものでないか、group / other が書き込めるなら断る: 別の uid が開いた後の
    /// socket を消して偽物に置き換えられ、peer の uid を見ない CLI に偽の答えを返せる。
    ///
    /// 脇の `<path>.lock` を flock で握れた時だけ進む (握れなければ別の台がこの path を
    /// 開いている)。握った後で、前回の残骸 (誰も待っていない socket) を消してから開く。
    /// socket でないファイルと、誰かが待っている socket は消さずに断る
    /// (監督者の頼み口と同じ流儀、[`gateway_core::daemon::socket`])。
    pub fn bind(path: &Path) -> io::Result<Self> {
        // SAFETY: geteuid は引数を取らず、失敗しない。
        let owner = unsafe { libc::geteuid() };
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let meta = std::fs::metadata(dir)?;
        check_dir(meta.uid(), meta.mode(), owner).map_err(|reason| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{}: {reason}", dir.display()),
            )
        })?;

        let lock = hold(path)?;
        clear_stale(path)?;
        let listener = gateway_core::unix_socket::bind(path)?;
        // 制御口と同じく bind の後で閉じる。umask を触るとプロセス全体に効き、他の
        // スレッドが作るファイルまで巻き込む。閉じるまでの間に繋いだ相手も、
        // 受ける時の uid の検査で落ちる。
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            file: (meta.dev(), meta.ino()),
            owner,
            _lock: lock,
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

    /// カーネルが答える socket の名前。path が長いと、開いた時の path ではない
    /// (相対パスやファイル名だけ等、[`gateway_core::unix_socket`])。開いた path は
    /// [`OperationsSocket::path`] が持つ。
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// `path` の運用系の口に 1 つの request を送り、応答を返す。
///
/// request の URI は path と query だけ (`/llm-gateway/usage?refresh=true`) を書く。
/// `Host` が無ければ `localhost` を足す (台は host を見ないが、HTTP/1.1 は必須とする)。
/// 返るのはヘッダまでで、本文は読み手が読む (`events` のように流れ続ける口もある)。
/// 繋がらない時は繋ぐ時の `io::Error` (`NotFound` / `ConnectionRefused` 等) をそのまま、
/// HTTP を話せなかった時はその理由を包んだ `io::Error` を返す。
///
/// Design rationale: HTTP の client は reqwest でなく hyper を直に使う。reqwest の
/// `unix_socket` は内部で tokio の connect を呼び、path が `sun_path` に収まらないと
/// 繋げない。connect を差し替える口 (`connector_layer`) も、包めるのは reqwest 自身の
/// connector だけで、返す接続の型が外から作れない。1 本の接続で 1 往復するだけなので、
/// sockat で繋いだ stream の上で HTTP/1 の handshake から行う。
pub async fn request(path: &Path, mut request: Request<Body>) -> io::Result<Response<Body>> {
    if !request.headers().contains_key(header::HOST) {
        request
            .headers_mut()
            .insert(header::HOST, header::HeaderValue::from_static("localhost"));
    }
    let stream = gateway_core::unix_socket::connect(path)?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(io::Error::other)?;
    // 接続を回す側。sender (この関数を出る時に落ちる) と応答の本文が落ちれば終わる。
    tokio::spawn(connection);
    let response = sender
        .send_request(request)
        .await
        .map_err(io::Error::other)?;
    Ok(response.map(Body::new))
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

/// socket を置くディレクトリとして信じてよいか。持ち主が自分で、group / other が
/// 書き込めないこと。sticky でも断る (sticky は他人のファイルを消させないだけで、
/// 同じディレクトリに別の名前を作ることは許し、`<path>.lock` を先に作られうる)。
fn check_dir(uid: libc::uid_t, mode: u32, owner: libc::uid_t) -> Result<(), String> {
    if uid != owner {
        return Err(format!(
            "the directory is owned by uid {uid}, not by the unit's uid {owner}; put the socket in a directory of your own"
        ));
    }
    if mode & 0o022 != 0 {
        return Err(format!(
            "the directory is writable by others (mode {:o}); put the socket in a directory only you can write",
            mode & 0o7777
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;
    use tempfile::TempDir;

    /// どの OS の `sun_path` (NUL 込みで macOS 104 / Linux 108 バイト) にも収まらない
    /// 深さの path。置き場の `$TMPDIR` が短い環境でも長くなるよう、深さを足して作る。
    fn beyond_sun_path(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("d".repeat(120)).join("gw.sock");
        assert!(path.as_os_str().len() > 108, "{}", path.display());
        path
    }

    /// 運用系の口は `sun_path` に収まらない path でも開き、CLI の読み手 ([`request`]) は
    /// そこへ繋いで HTTP を話せる。socket ファイルは書いた path そのものにでき、落とせば
    /// そこから消える。
    ///
    /// 置き場 (`[server] unix`、既定は `$XDG_STATE_HOME` の下) の深さは運用者と環境が
    /// 決めるので、長さで断ると「TCP は動くのに CLI が台に届かない」台ができる。開く側と
    /// 読む側のどちらかが std / reqwest の connect に戻ると、ここで落ちる。
    #[tokio::test]
    async fn a_path_longer_than_sun_path_is_opened_and_read() {
        let dir = TempDir::new().unwrap();
        let path = beyond_sun_path(dir.path());

        let socket = OperationsSocket::bind(&path).unwrap();
        assert_eq!(socket.path(), path, "keeps the path it was given");
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_socket()
        );
        let router = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let serving = tokio::spawn(async move {
            let _ = axum::serve(socket, router.into_make_service()).await;
        });

        let get = Request::get("/ping").body(Body::empty()).unwrap();
        let answer = request(&path, get).await.unwrap();
        assert_eq!(answer.status(), 200);
        let body = axum::body::to_bytes(answer.into_body(), 64).await.unwrap();
        assert_eq!(body.as_ref(), b"pong");

        serving.abort();
        let _ = serving.await;
        assert!(!path.exists(), "the socket file is removed by its path");
    }

    /// 誰も開いていない path へは、繋ぐ時の理由 (`NotFound`) がそのまま返る。CLI は
    /// これを「台に届かない」の理由として見せる。
    #[tokio::test]
    async fn requesting_a_missing_socket_says_it_is_not_there() {
        let dir = TempDir::new().unwrap();
        let get = Request::get("/ping").body(Body::empty()).unwrap();
        let e = request(&dir.path().join("absent.sock"), get)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

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

    /// socket を置くディレクトリは、持ち主が自分で group / other が書き込めないものだけ。
    ///
    /// 持ち主が別の uid のディレクトリを実際に用意する試験は root が要るので、検査の
    /// 関数で確かめる。
    #[test]
    fn only_a_private_directory_is_trusted() {
        assert_eq!(check_dir(501, 0o40700, 501), Ok(()));
        assert_eq!(check_dir(501, 0o40755, 501), Ok(()));

        let foreign = check_dir(0, 0o40755, 501).unwrap_err();
        assert!(foreign.contains("uid 0"), "{foreign}");
        for mode in [0o40770, 0o40777, 0o41777, 0o40702] {
            let open = check_dir(501, mode, 501).unwrap_err();
            assert!(open.contains("writable by others"), "{mode:o}: {open}");
        }
    }

    /// group / other が書き込めるディレクトリ (sticky でも) には socket を開かない。
    #[tokio::test]
    async fn a_directory_others_can_write_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("gw.sock");
        for mode in [0o777, 0o1777, 0o770] {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();

            let e = OperationsSocket::bind(&path).err().expect("refused");
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{mode:o}: {e}");
            assert!(e.to_string().contains("writable by others"), "{e}");
            assert!(!path.exists(), "{mode:o}: nothing is bound");
        }
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// 同じ path を 2 台が同時に開こうとしても、開けるのは片方だけ。もう片方は
    /// `AddrInUse` で断られ、先に開いた側の socket は生きたまま。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_units_racing_for_one_path_do_not_steal_it() {
        let dir = TempDir::new().unwrap();
        for round in 0..50 {
            let path = dir.path().join(format!("gw-{round}.sock"));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let racers: Vec<_> = (0..2)
                .map(|_| {
                    let (path, barrier) = (path.clone(), std::sync::Arc::clone(&barrier));
                    tokio::task::spawn_blocking(move || {
                        barrier.wait();
                        OperationsSocket::bind(&path)
                    })
                })
                .collect();
            let mut bound = Vec::new();
            let mut refused = Vec::new();
            for racer in racers {
                match racer.await.unwrap() {
                    Ok(socket) => bound.push(socket),
                    Err(e) => refused.push(e),
                }
            }
            assert_eq!(bound.len(), 1, "round {round}: {refused:?}");
            assert_eq!(refused.len(), 1, "round {round}");
            assert_eq!(
                refused[0].kind(),
                io::ErrorKind::AddrInUse,
                "round {round}: {}",
                refused[0]
            );
            gateway_core::unix_socket::connect(&path)
                .unwrap_or_else(|e| panic!("round {round}: the winner still listens: {e}"));
        }
    }

    /// 開いた台が手放せば lock は外れ、次の台が同じ path を開ける。
    #[tokio::test]
    async fn the_path_is_free_again_once_the_holder_drops_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("gw.sock");
        drop(OperationsSocket::bind(&path).unwrap());
        let _again = OperationsSocket::bind(&path).unwrap();
    }

    /// 開いた socket は 0600 で、親ディレクトリが無ければ 0700 で作る。落とすと消える。
    #[tokio::test]
    async fn the_socket_is_private_and_removed_when_dropped() {
        let dir = TempDir::new().unwrap();
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
        let dir = TempDir::new().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let _socket = OperationsSocket::bind(&dir.path().join("gw.sock")).unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    /// 前回落ちた台が残した socket (誰も待っていない) は片付けて開ける。
    #[tokio::test]
    async fn a_stale_socket_is_cleared() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("gw.sock");
        // 待ち受けを閉じても、ファイルは消えない (std と同じ。落ちた台の残骸)。
        drop(sockat::bind(&path).unwrap());
        assert!(path.exists());

        let socket = OperationsSocket::bind(&path).unwrap();
        gateway_core::unix_socket::connect(&path).expect("the new socket listens");
        drop(socket);
    }

    /// 誰かが待っている socket は消さずに断る (同じ path を書いた 2 台の取り合い)。
    #[tokio::test]
    async fn a_socket_someone_listens_on_is_not_taken() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("gw.sock");
        let first = OperationsSocket::bind(&path).unwrap();

        let e = OperationsSocket::bind(&path).err().expect("refused");
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse, "{e}");
        gateway_core::unix_socket::connect(&path).expect("the first one still listens");
        drop(first);
    }

    /// socket でないファイルは、path の書き間違いを疑って消さない。
    #[tokio::test]
    async fn a_file_that_is_not_a_socket_is_left_alone() {
        let dir = TempDir::new().unwrap();
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
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("gw.sock");
        let socket = OperationsSocket::bind(&path).unwrap();
        // 退けるだけで消さない (inode が生きている間は、置き直す socket に同じ番号が付かない)。
        std::fs::rename(&path, dir.path().join("moved.sock")).unwrap();
        let _other = sockat::bind(&path).unwrap();

        drop(socket);
        assert!(path.exists(), "the other socket stays");
    }
}
