//! 試験の部品。利用側の crate の試験からは feature `test-support` で使う。
//!
//! - [`SocketTempDir`] Unix socket を置ける一時ディレクトリ

use std::io;
use std::path::Path;

/// Unix socket を置ける一時ディレクトリ。落とすと片付く。
///
/// bind / connect に渡す path 文字列は sun_path (macOS 104 / Linux 108 バイト) に
/// 収まらないと断られる。一時ディレクトリの置き場 (`$TMPDIR`) はそれより長いことがある
/// (作業ごとに深い置き場を割り当てる環境など)。そこで実体は `$TMPDIR` の下に作り、
/// `/tmp` に置いた短いランダム名の symlink を [`path`](Self::path) として渡す。
/// symlink は文字列を短くするだけで、置いたファイルはどれも実体の側にできる。
///
/// Design rationale: 試験の一時ファイルは `$TMPDIR` に置くのが流儀だが、`$TMPDIR` の
/// 長さは試験からは選べず、短いことに頼ると置き場次第で落ちる。cwd を合わせて相対
/// path で渡す手は、cwd がプロセス全体で 1 つで試験が並行に走るので使えず、子
/// プロセスへ渡す `XDG_STATE_HOME` は絶対 path でなければならない。どの環境にもある
/// 短い絶対 path の `/tmp` には symlink 1 つだけを置き、実体は `$TMPDIR` に残す。
pub struct SocketTempDir {
    // 欄は書いた順に落ちる。symlink を先に消し、実体はその後で消す。
    link: tempfile::TempPath,
    _dir: tempfile::TempDir,
}

impl SocketTempDir {
    pub fn new() -> io::Result<Self> {
        let dir = tempfile::TempDir::new()?;
        let link = tempfile::Builder::new()
            .prefix("lgw.")
            .make_in("/tmp", |link| std::os::unix::fs::symlink(dir.path(), link))?
            .into_temp_path();
        Ok(Self { link, _dir: dir })
    }

    /// 実体を指す短い path。socket はこの下に置く。
    pub fn path(&self) -> &Path {
        &self.link
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 渡す path の長さは `$TMPDIR` の長さに依らず `/tmp/lgw.` + ランダム 6 文字で、
    /// 試験が置く深さ (状態ディレクトリの下の制御口) の socket を開いて繋げる。
    #[test]
    fn the_path_is_short_whatever_the_temporary_directory_is() {
        let dir = SocketTempDir::new().unwrap();
        assert_eq!(dir.path().as_os_str().len(), "/tmp/lgw.".len() + 6);

        let socket = dir
            .path()
            .join("state/llm-gateway/daemon/control/unit.sock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();

        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::os::unix::net::UnixStream::connect(&socket).unwrap();
    }

    /// 置いたファイルは実体の側にでき、落とすと symlink も実体も残らない。
    #[test]
    fn dropping_removes_both_the_link_and_what_it_points_at() {
        let dir = SocketTempDir::new().unwrap();
        let link = dir.path().to_path_buf();
        let target = std::fs::read_link(&link).unwrap();
        std::fs::write(link.join("a"), "").unwrap();
        assert!(target.join("a").exists());

        drop(dir);
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "the link is left"
        );
        assert!(!target.exists(), "the directory is left");
    }
}
