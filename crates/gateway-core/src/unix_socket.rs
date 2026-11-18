//! Unix socket を tokio の型で開く・繋ぐ。path の長さは問わない。
//!
//! bind / connect は [`sockat`] を通す。std と tokio は path が `sun_path` (macOS 104 /
//! Linux 108 バイト) に収まらないと断るが、置き場 (`$XDG_STATE_HOME`、`[server] unix`、
//! 試験の `$TMPDIR`) の深さは利用者と環境が決め、こちらからは選べない。sockat は長い
//! path にも、渡した path そのものに socket を作る (繋ぐ時はそこを引く)。
//!
//! 長い path で std と違う点のうち、使う側が気にするのは 2 つ:
//!
//! - **`local_addr()` / `peer_addr()` は渡した path を返さない** (相対パス、
//!   `/proc/self/fd/...`、消えた symlink、ファイル名だけ等)。消す・表示するには、
//!   渡した path を自分で持って使う
//! - **connect は fork しない**。ふつうの環境 (`$TMPDIR` か `/tmp` に短い symlink を
//!   置ける) ならどの深さの path にも届き、fork でしか届かない珍しい環境 (`/proc` も
//!   安全な一時ディレクトリも無い等) では `ENAMETOOLONG` で断る。fork してでも繋ぐ
//!   `sockat::connect_any` は使わない: その経路で繋ぐと受けた側に見える相手の pid が
//!   既に終わった子になり、届かない環境は珍しいので、その副作用を負ってまで繋ぐ理由が
//!   無い (DR-0028 決定 7)

use std::io;
use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

/// `path` で待ち受ける。socket ファイルは `path` にでき、閉じても消えない (std と同じ)。
///
/// tokio の runtime の中で呼ぶ (`from_std` が reactor に登録する)。
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    let listener = sockat::bind(path)?;
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

/// `path` に繋ぐ。
///
/// `sockat::connect_nonblocking` は待たない (Unix domain の connect は呼んだ中で
/// 終わり、相手の backlog が満杯なら Linux ではすぐ `WouldBlock`、macOS では
/// `ConnectionRefused` で返る) ので、runtime の worker の上でそのまま呼ぶ。
///
/// tokio の runtime の中で呼ぶ (`from_std` が reactor に登録する)。
pub fn connect(path: &Path) -> io::Result<UnixStream> {
    UnixStream::from_std(sockat::connect_nonblocking(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// どの OS の `sun_path` (NUL 込みで最大 108 バイト) にも収まらない深さの path を
    /// 作る。置き場の `$TMPDIR` が短い環境でも長くなるよう、深さは足して作る。
    fn beyond_sun_path(dir: &Path) -> std::path::PathBuf {
        let deep = dir.join("d".repeat(120));
        std::fs::create_dir_all(&deep).unwrap();
        let path = deep.join("s.sock");
        assert!(path.as_os_str().len() > 108, "{}", path.display());
        path
    }

    /// `sun_path` を超える path でも開いて繋がり、socket ファイルは渡した path にできる。
    #[tokio::test]
    async fn a_path_longer_than_sun_path_is_bound_and_connected() {
        let dir = tempfile::tempdir().unwrap();
        let path = beyond_sun_path(dir.path());

        let listener = bind(&path).unwrap();
        let mut client = connect(&path).unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut got = [0; 4];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");

        use std::os::unix::fs::FileTypeExt;
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_socket()
        );
    }

    /// 繋げない理由は std と同じ種類で返る (呼ぶ側は `NotFound` / `ConnectionRefused`
    /// で「居ない」を見分ける)。
    #[tokio::test]
    async fn connecting_to_nothing_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = beyond_sun_path(dir.path());

        let absent = connect(&path).unwrap_err();
        assert_eq!(absent.kind(), io::ErrorKind::NotFound, "{absent}");

        drop(bind(&path).unwrap());
        let refused = connect(&path).unwrap_err();
        assert_eq!(
            refused.kind(),
            io::ErrorKind::ConnectionRefused,
            "{refused}"
        );
    }
}
