//! 監督者に頼むときの言葉 (DR-0028 決定 3)。
//!
//! 頼み先は unix socket 1 本。要求は JSON 1 行、答えも JSON 1 行で、
//! `log` の追従だけは答えが JSONL として続く。
//!
//! 監督者が居なければ socket も無い。CLI はそれを「監督者が動いていない」と
//! して断り、代わりに子を起こしたりはしない (所有者が 2 つになる)。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 頼みごと。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// 居てほしい状態にして、居なければ起こす。
    Start(Which),
    /// 居てほしくない状態にして、居るなら止める。
    Stop(Which),
    /// 止めてから起こす。複数なら 1 台ずつ (DR-0028 決定 4)。
    Restart(Which),
    /// どうしているか。
    Status(Which),
    /// 登録簿を読み直して、望みとの差を埋める。
    Reload,
    /// 書いたものを流し続ける (答えは JSONL)。
    Log(Which),
}

/// どの台に対しての頼みか。
///
/// `--all` と名前指定を 1 つの形にまとめる。どちらも無い場合の既定は
/// 命令によって違う (`status` は全部、`start` は指せと言う) ので、
/// ここでは決めずに [`Which::choose`] の呼び手が渡す。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Which {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
}

impl Which {
    pub fn all() -> Self {
        Self {
            unit: None,
            all: true,
        }
    }

    pub fn named(name: impl Into<String>) -> Self {
        Self {
            unit: Some(name.into()),
            all: false,
        }
    }

    /// 登録されている名前から、対象を選ぶ。
    ///
    /// `bare_is_all` は「名前も `--all` も無いとき全部とみなすか」。
    /// 数えるだけの `status` / `log` は全部でよいが、動かす命令は
    /// 指されていないものを勝手に動かさない。
    pub fn choose(&self, registered: &[String], bare_is_all: bool) -> Result<Vec<String>, String> {
        if let Some(name) = &self.unit {
            if self.all {
                return Err("say a unit or --all, not both".to_owned());
            }
            if !registered.iter().any(|r| r == name) {
                return Err(format!("there is no unit called `{name}`"));
            }
            return Ok(vec![name.clone()]);
        }
        if self.all || bare_is_all {
            return Ok(registered.to_vec());
        }
        Err("say which unit, or --all".to_owned())
    }
}

/// 1 台の今 (DR-0028 決定 5: プロセスの状態)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UnitStatus {
    /// 登録簿の並び順 (名前順) での番号。
    pub id: usize,
    pub unit: String,
    /// 居てほしいか (登録簿の desired state)。
    pub enabled: bool,
    /// 今いるか。
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 今の子が起きた時刻 (epoch ミリ秒)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// 今動いているプロセスが載せている版。
    ///
    /// ディスクの binary ではなく、走っている本人 (`GET /llm-gateway/version`)
    /// が答えたもの。答えられない版が走っていることもあるので `null` を許す。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// 監督者が起こし直した回数。
    pub restarts: u32,
    /// 最後に終わったときの様子。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<String>,
}

/// 子が書いた 1 行。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LogLine {
    pub unit: String,
    pub line: String,
    /// この行を書き終えた時点のログの長さ (バイト)。
    ///
    /// 追従は「ファイルを読む」と「流れてくる」の 2 経路で同じ行に届きうる。
    /// どこまで読んだかをバイト数で言えるので、既に読んだ行を捨てられる。
    pub offset: u64,
}

/// 監督者の待ち受け先。
///
/// 状態ディレクトリに置く。cache に置くと、掃除された拍子に「監督者は
/// 居るのに繋げない」が起きる。
pub fn socket_path() -> PathBuf {
    crate::config::default_state_dir()
        .join("daemon")
        .join("supervisor.sock")
}

/// 子が書いたものの置き場。
pub fn log_dir() -> PathBuf {
    crate::config::default_state_dir().join("logs")
}

/// 1 台ぶんのログ。
pub fn log_path(dir: &Path, unit: &str) -> PathBuf {
    dir.join(format!("{unit}.log"))
}

/// 頼んで、1 行の答えを受け取る。
pub async fn ask(socket: &Path, request: &Request) -> std::io::Result<String> {
    let stream = UnixStream::connect(socket).await?;
    let mut lines = send(stream, request).await?;
    match lines.next_line().await? {
        Some(line) => Ok(line),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the supervisor closed the connection without answering",
        )),
    }
}

/// 頼みを書いて、答えを読む口を返す。`log` はここから流れ続ける。
pub async fn send(
    stream: UnixStream,
    request: &Request,
) -> std::io::Result<tokio::io::Lines<BufReader<UnixStream>>> {
    let mut stream = stream;
    let mut text = serde_json::to_string(request)?;
    text.push('\n');
    stream.write_all(text.as_bytes()).await?;
    stream.flush().await?;
    Ok(BufReader::new(stream).lines())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 要求は `op` で分かれた 1 つの物として読める。
    #[test]
    fn a_request_is_one_object_named_by_its_op() {
        let text = serde_json::to_string(&Request::Start(Which::named("stable"))).unwrap();
        assert_eq!(text, r#"{"op":"start","unit":"stable"}"#);

        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"restart","all":true}"#).unwrap(),
            Request::Restart(Which::all())
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"status"}"#).unwrap(),
            Request::Status(Which::default())
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"reload"}"#).unwrap(),
            Request::Reload
        );
    }

    /// 名前で指せば 1 台、`--all` なら登録順のまま全部。
    #[test]
    fn a_name_picks_one_and_all_picks_everything() {
        let registered = vec!["stable".to_owned(), "unstable".to_owned()];

        assert_eq!(
            Which::named("stable").choose(&registered, false).unwrap(),
            vec!["stable".to_owned()]
        );
        assert_eq!(Which::all().choose(&registered, false).unwrap(), registered);
    }

    /// 動かす命令は、指されていない台を勝手に動かさない。
    #[test]
    fn nothing_is_moved_unless_it_was_pointed_at() {
        let registered = vec!["a".to_owned(), "b".to_owned()];

        let e = Which::default().choose(&registered, false).unwrap_err();
        assert!(e.contains("--all"), "{e}");
        // 数えるだけの命令は、指されなければ全部でよい。
        assert_eq!(
            Which::default().choose(&registered, true).unwrap(),
            registered
        );
    }

    /// 知らない名前と、両方指した形は断る。
    #[test]
    fn an_unknown_or_doubled_target_is_refused() {
        let registered = vec!["a".to_owned()];

        assert!(
            Which::named("nope")
                .choose(&registered, false)
                .unwrap_err()
                .contains("nope")
        );
        let both = Which {
            unit: Some("a".to_owned()),
            all: true,
        };
        assert!(
            both.choose(&registered, false)
                .unwrap_err()
                .contains("both")
        );
    }

    /// 待ち受け先とログは、消えると困るので state の下 (cache ではない)。
    #[test]
    fn the_supervisor_lives_under_the_state_directory() {
        assert!(
            socket_path().ends_with("llm-gateway/daemon/supervisor.sock"),
            "{}",
            socket_path().display()
        );
        assert!(
            log_dir().ends_with("llm-gateway/logs"),
            "{}",
            log_dir().display()
        );
        assert_eq!(
            log_path(Path::new("/var/log"), "stable"),
            PathBuf::from("/var/log/stable.log")
        );
    }
}
