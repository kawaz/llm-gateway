//! 監督者に頼むときの言葉 (DR-0028 決定 3)。
//!
//! 頼み先は unix socket 1 本。要求は JSON 1 行、答えも JSON 1 行で、
//! `log` の追従だけは答えが JSONL として続く。
//!
//! 監督者が居なければ socket も無い。CLI はそれを「監督者が動いていない」と
//! して断り、代わりに子を起こしたりはしない (所有者が 2 つになる)。

use std::collections::BTreeMap;
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
    /// 登録簿を読み直して望みとの差を埋め、指された台に設定を読み直させる
    /// (DR-0032 決定 2)。
    Reload(Which),
    /// 書いたものを流し続ける (答えは JSONL)。
    Log(Which),
    /// 指した台に一時ダンプを頼む (DR-0038 決定 1)。頼みの中身は台が解釈し、
    /// 監督者は制御口へ渡すだけ。
    Dump(DumpRelay),
    /// `dump start --follow` (DR-0038 決定 12)。答えは 1 行で終わらず、台が流す
    /// 行をそのまま通し続ける。頼んだ側が繋がりを閉じたら台への繋がりも閉じる。
    DumpFollow(DumpRelay),
}

/// 監督者に渡す一時ダンプの頼み。どの台かは CLI が登録簿から決めて書く
/// (DR-0038 決定 3、走っている台に聞く他の命令と同じ指し方)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DumpRelay {
    pub unit: String,
    pub dump: DumpRequest,
}

/// 台の制御口が受ける一時ダンプの操作 (DR-0038 決定 1)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DumpRequest {
    /// 1 つの ns の系列を始める。書かなかった値は台の `[dump]` の既定。
    Start {
        ns: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        store: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        labels: BTreeMap<String, String>,
        /// 接続を保ってイベントを流す。
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        follow: bool,
        /// follow で本文も同梱する (`bodies`)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embed: Option<String>,
        /// 保存するか (既定 `true`)。`false` は follow + embed の時だけ。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        persist: Option<bool>,
    },
    /// 系列を止める。冪等。
    Stop { dump_id: String },
    /// 系列の一覧。選択子 (`k=v` / `k!=v` / `k`) に当たるものだけ。
    List {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        selectors: Vec<String>,
    },
    /// 系列をディレクトリごと消す。`dump_ids` か選択子のどちらかが要る。
    Prune {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dump_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        selectors: Vec<String>,
    },
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
    /// ディスクの binary ではなく、走っている本人 (利用側が決めた自己申告の口)
    /// が答えたもの。答えられない版が走っていることもあるので `null` を許す。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 走っている本人が数えた、知らせ (DR-0012) の配り損ね。版と同じ問い合わせ
    /// で聞く。答えられなければ欄ごと出さない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<UnitEvents>,
    /// 走っている本人が答えた、動いている一時ダンプの系列 (DR-0038 決定 12)。
    /// 1 つも動いていなければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dump: Option<Vec<UnitDump>>,

    /// 監督者が起こし直した回数。
    pub restarts: u32,
    /// 最後に終わったときの様子。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<String>,
    /// 台の待ち受けポートを今だれが握っているか (DR-0028 決定 11)。
    ///
    /// 待ち受け先が読めない台には聞きに行かないので欄ごと出さない。この欄を
    /// 持たない監督者の答えも読めるよう、無くてもよい。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<PortHolder>,
}

/// 台の待ち受けポートの様子。
///
/// 監督者が落ちて残った子がポートを握っていると、新しい子は待ち受けられずに
/// 落ち続ける。その時 `running: false` と `restarts` の増加だけでは「なぜ上がら
/// ないか」が分からないので、ポートを握っている者が自分の子かどうかを並べる。
/// 握っている者を引き取ったり畳んだりはしない (決定 11)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PortHolder {
    /// 設定に書かれた待ち受け先。
    pub listen: String,
    /// 誰かがそこで待ち受けているか (繋いでみて決める)。
    pub held: bool,
    /// 待ち受けで握っているプロセスの pid。誰も握っていなければ空。握っているが
    /// 誰かを引けない (`lsof` が無い等) なら `null`。
    pub pids: Option<Vec<u32>>,
    /// 握っているのが監督者の今の子ではないか。子が居ないのに塞がっていれば
    /// `true`、握っている pid が分かって今の子が含まれていなければ `true`。
    /// 空いていれば `false`。子が居て pid が分からなければ `null` (比べられない)。
    pub foreign: Option<bool>,
}

/// 1 台が数えた知らせの配り損ね (DR-0012)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UnitEvents {
    /// 見る側が追いつけずに落とした数。その台の起動からの累積。
    pub dropped: u64,
}

/// 1 台で動いている一時ダンプの系列 1 つ (DR-0038 決定 12)。
///
/// 止め忘れに気づくための欄で、本文の置き場は載せない (`dump status` が答える)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UnitDump {
    pub dump_id: String,
    pub ns: String,
    /// 時限で止まる時刻 (Unix ミリ秒)。
    pub expires_at: i64,
    pub bytes_written: u64,
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

/// 監督者が子の制御口に頼むこと (DR-0032 決定 2)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    /// 設定を読み直す。
    Reload,
    /// 一時ダンプを操作する (DR-0038 決定 2)。
    Dump(DumpRequest),
}

/// 1 台の読み直しの結果 (DR-0032 決定 5)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Reloaded {
    pub unit: String,
    /// 新しい設定に差し替わったか。`false` なら旧設定のまま走っている。
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReloadFailure>,
    /// 差し替えは済んだが、気に留めてほしいこと。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// 読み直せなかった理由。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReloadFailure {
    pub kind: String,
    pub message: String,
    /// `restart_required` のとき、変わっていた欄の名前。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
}

impl Reloaded {
    pub fn done(unit: &str, warnings: Vec<String>) -> Self {
        Self {
            unit: unit.to_owned(),
            ok: true,
            error: None,
            warnings,
        }
    }

    pub fn failed(unit: &str, kind: &str, message: impl Into<String>) -> Self {
        Self {
            unit: unit.to_owned(),
            ok: false,
            error: Some(ReloadFailure {
                kind: kind.to_owned(),
                message: message.into(),
                fields: Vec::new(),
            }),
            warnings: Vec::new(),
        }
    }
}

/// 監督者の待ち受け先。
///
/// 状態ディレクトリに置く。消してよい一時置き場に置くと、掃除された拍子に「監督者は
/// 居るのに繋げない」が起きる。
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon").join("supervisor.sock")
}

/// 子が書いたものの置き場。
pub fn log_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("logs")
}

/// 1 台ぶんのログ。
pub fn log_path(dir: &Path, unit: &str) -> PathBuf {
    dir.join(format!("{unit}.log"))
}

/// 子の制御口の置き場 (DR-0032 決定 2)。
///
/// 子の HTTP とは分け、外から届かない unix socket にだけ置く。置き場は
/// 状態ディレクトリと unit の名前で決まるので、子にも監督者にも教えない。
pub fn control_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon").join("control")
}

/// 1 台ぶんの制御口。
pub fn control_path(dir: &Path, unit: &str) -> PathBuf {
    dir.join(format!("{unit}.sock"))
}

/// 頼んで、1 行の答えを受け取る。
pub async fn ask<T: Serialize>(socket: &Path, request: &T) -> std::io::Result<String> {
    let stream = UnixStream::connect(socket).await?;
    let mut lines = send(stream, request).await?;
    match lines.next_line().await? {
        Some(line) => Ok(line),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!(
                "{} closed the connection without answering",
                socket.display()
            ),
        )),
    }
}

/// 頼みを書いて、答えを読む口を返す。`log` はここから流れ続ける。
pub async fn send<T: Serialize>(
    stream: UnixStream,
    request: &T,
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
            serde_json::from_str::<Request>(r#"{"op":"reload","unit":"stable"}"#).unwrap(),
            Request::Reload(Which::named("stable"))
        );
    }

    /// ダンプの頼みは、監督者宛ても制御口宛ても 1 行の 1 つの物として読める。
    #[test]
    fn a_dump_request_is_one_object_both_ways() {
        let start = DumpRequest::Start {
            ns: "lab".to_owned(),
            timeout_ms: Some(600_000),
            max_bytes: None,
            store: None,
            labels: BTreeMap::from([("owner".to_owned(), "kawaz".to_owned())]),
            follow: false,
            embed: None,
            persist: None,
        };
        let relay = Request::Dump(DumpRelay {
            unit: "unstable".to_owned(),
            dump: start.clone(),
        });
        let text = serde_json::to_string(&relay).unwrap();
        assert_eq!(
            text,
            r#"{"op":"dump","unit":"unstable","dump":{"action":"start","ns":"lab","timeout_ms":600000,"labels":{"owner":"kawaz"}}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), relay);

        let control = ControlRequest::Dump(start);
        let text = serde_json::to_string(&control).unwrap();
        assert_eq!(
            serde_json::from_str::<ControlRequest>(&text).unwrap(),
            control
        );
        assert_eq!(
            serde_json::from_str::<ControlRequest>(
                r#"{"op":"dump","action":"stop","dump_id":"x"}"#
            )
            .unwrap(),
            ControlRequest::Dump(DumpRequest::Stop {
                dump_id: "x".to_owned()
            })
        );
        assert_eq!(
            serde_json::from_str::<ControlRequest>(r#"{"op":"dump","action":"list"}"#).unwrap(),
            ControlRequest::Dump(DumpRequest::List {
                selectors: Vec::new()
            })
        );
        assert_eq!(
            serde_json::from_str::<ControlRequest>(
                r#"{"op":"dump","action":"prune","selectors":["owner=kawaz"]}"#
            )
            .unwrap(),
            ControlRequest::Dump(DumpRequest::Prune {
                dump_ids: Vec::new(),
                selectors: vec!["owner=kawaz".to_owned()]
            })
        );
    }

    /// 読み直しの答えは、成功なら `ok` だけ、失敗なら理由を添える。
    #[test]
    fn a_reload_result_carries_only_what_it_has() {
        assert_eq!(
            serde_json::to_string(&Reloaded::done("a", Vec::new())).unwrap(),
            r#"{"unit":"a","ok":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Reloaded::failed("a", "invalid_config", "bad")).unwrap(),
            r#"{"unit":"a","ok":false,"error":{"kind":"invalid_config","message":"bad"}}"#
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

    /// 待ち受け先とログは、消えると困るので state の下 (消してよい一時置き場ではない)。
    #[test]
    fn the_supervisor_lives_under_the_state_directory() {
        let state = Path::new("/state/app");
        assert_eq!(
            socket_path(state),
            PathBuf::from("/state/app/daemon/supervisor.sock")
        );
        assert_eq!(log_dir(state), PathBuf::from("/state/app/logs"));
        assert_eq!(
            control_path(&control_dir(state), "stable"),
            PathBuf::from("/state/app/daemon/control/stable.sock")
        );
        assert_eq!(
            log_path(Path::new("/var/log"), "stable"),
            PathBuf::from("/var/log/stable.log")
        );
    }
}
