//! 台の待ち受けポートを、今だれが握っているか (DR-0028 決定 11)。
//!
//! 監督者が SIGKILL などで消えると、子は残ってポートを握り続ける。次の監督者が
//! 起こした子はそのポートで待ち受けられずに落ち、backoff に入る。決定 11 は
//! 残った子を引き取りも畳みもしない (pid は使い回されるので、誰の子かを確かめ
//! られない) と決めているので、ここがするのは**塞がっていることと、握っている
//! pid を見せる**ところまで。畳むのは人 (runbook `docs/runbooks/supervisor-orphan-children.md`)。
//!
//! 2 段に分けて調べる:
//!
//! - **塞がっているか**は、待ち受け先へ TCP で繋いでみて決める。繋がれば誰かが
//!   待ち受けている。OS を問わず、外のコマンドにも頼らない
//! - **誰が握っているか**は `lsof` に聞く。答えられなければ `None` (= 分からない)
//!   にして、塞がっていることだけを言う
//!
//! Design rationale: pid の引き方を `lsof` に任せ、OS の API (macOS の
//! `proc_pidfdinfo`、Linux の `/proc/net/tcp` と `/proc/<pid>/fd`) を自前で
//! 叩かないのは、OS ごとに別の実装と別の試験が要るうえ、どちらも libc の
//! 薄い包みでは揃わず crate か unsafe の構造体定義を足すことになるため。
//! 使うのは人が事故の時に見る場面だけで、毎リクエストの道ではないので、外の
//! コマンドを 1 回起こす重さは問題にならない。`lsof` は macOS には必ずあり、
//! Linux では入っていないことがあるが、その時も「塞がっている」までは言える
//! (pid が `null` になるだけ)。

use std::time::Duration;

use super::protocol::PortHolder;

/// 塞がっているかを確かめる接続の待ち上限。
///
/// 同じ機械の上の待ち受けなので、繋がるなら即座に繋がる。状態を出す道の
/// 途中なので、長く待つと `daemon status` 自体が返らなくなる。
const CONNECT_WAIT: Duration = Duration::from_millis(500);

/// `lsof` の答えを待つ上限。
///
/// 手元の macOS で 1 回 0.1 秒ほど。名前の逆引き (`-n` / `-P` で切っている)
/// のような止まる要因を避けても返らないなら、分からないことにする。
const LSOF_WAIT: Duration = Duration::from_secs(3);

/// 待ち受け先の書き方 (`host:port` / `[::1]:port`) からポート番号を取る。
pub fn port_of(listen: &str) -> Option<u16> {
    listen.rsplit_once(':')?.1.parse().ok()
}

/// その待ち受け先で、誰かが待ち受けているか。
///
/// `0.0.0.0` / `::` は繋ぎに行く先ではないので、同じ機械の `127.0.0.1` に
/// 読み替えて繋ぐ (監督者が healthz を叩くときと同じ読み替え)。
pub async fn is_held(listen: &str) -> bool {
    let authority = super::supervisor::reachable_authority(listen);
    // 名前の引きも含めて上限の内に収める (`localhost:11301` のような書き方もある)。
    matches!(
        tokio::time::timeout(
            CONNECT_WAIT,
            tokio::net::TcpStream::connect(authority.as_str())
        )
        .await,
        Ok(Ok(_))
    )
}

/// そのポートを待ち受けで握っているプロセスの pid。
///
/// `lsof` が無い・時間内に答えない・読めない答えを返した時は `None`
/// (= 分からない)。握っている者が居ないと答えた時は空の配列。
pub async fn holders(port: u16) -> Option<Vec<u32>> {
    let run = tokio::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(LSOF_WAIT, run).await.ok()?.ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    // 該当が無いとき lsof は何も書かずに 1 で終わる。書いたのに 0 でない
    // 時は、答えの一部しか取れていないかもしれないので信じない。
    if !output.status.success() {
        return text.trim().is_empty().then(Vec::new);
    }
    let mut pids: Vec<u32> = text
        .lines()
        .map(|line| line.trim().parse().ok())
        .collect::<Option<_>>()?;
    pids.sort_unstable();
    pids.dedup();
    Some(pids)
}

/// 待ち受け先を今だれが握っているかを、監督者の今の子 (`child`) と比べて答える。
///
/// 空いていれば `lsof` は起こさない (握っている者が居ないのは繋いで分かった)。
/// 塞がっているのに `lsof` が誰も挙げないのは、見えない相手 (別のユーザの
/// プロセス等) が握っている時で、空とは言わずに「分からない」にする。
pub async fn inspect(listen: &str, child: Option<u32>) -> PortHolder {
    if !is_held(listen).await {
        return PortHolder {
            listen: listen.to_owned(),
            held: false,
            pids: Some(Vec::new()),
            foreign: Some(false),
        };
    }
    let pids = match port_of(listen) {
        Some(port) => holders(port).await.filter(|pids| !pids.is_empty()),
        None => None,
    };
    let foreign = match (child, &pids) {
        // 子が居ないのに塞がっている。誰であれ自分の子ではない。
        (None, _) => Some(true),
        (Some(child), Some(pids)) => Some(!pids.contains(&child)),
        (Some(_), None) => None,
    };
    PortHolder {
        listen: listen.to_owned(),
        held: true,
        pids,
        foreign,
    }
}

/// 子が落ちた理由に添える、ポートを塞いでいる者の説明。塞がっていなければ `None`。
///
/// 落ちた後に調べるので、握っているのは誰であれ監督者の子ではない。
pub fn blocked_by(holder: &PortHolder) -> Option<String> {
    if !holder.held {
        return None;
    }
    let who = match &holder.pids {
        Some(pids) => format!(
            "pid {}",
            pids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => "a process that could not be identified".to_owned(),
    };
    let port = port_of(&holder.listen).map_or_else(|| holder.listen.clone(), |p| p.to_string());
    Some(format!(
        "{} is held by {who}, not by a child of this supervisor \
(possibly left behind by a supervisor that died); find it with `lsof -nP -iTCP:{port} -sTCP:LISTEN`",
        holder.listen
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsof_is_here() -> bool {
        std::process::Command::new("lsof")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn the_port_is_read_from_the_listen_address() {
        assert_eq!(port_of("127.0.0.1:11301"), Some(11301));
        assert_eq!(port_of("0.0.0.0:80"), Some(80));
        assert_eq!(port_of("[::1]:11302"), Some(11302));
        assert_eq!(port_of("localhost"), None);
        assert_eq!(port_of("127.0.0.1:http"), None);
    }

    /// 待ち受けている先は塞がっていて、閉じた先は空いている。
    #[tokio::test]
    async fn a_listening_port_is_held_and_a_closed_one_is_not() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = listener.local_addr().unwrap().to_string();
        assert!(is_held(&listen).await);
        // `0.0.0.0` で書いてあっても、同じ機械の待ち受けとして確かめる。
        let port = port_of(&listen).unwrap();
        assert!(is_held(&format!("0.0.0.0:{port}")).await);

        // 閉じた先は、誰も待ち受けない番号で見る。今閉じたポートを見直すと、
        // 監督者の試験と並べて走らせた時に十数回に数回まだ繋がった (手元の
        // macOS、直後の lsof には誰も出ない)。同じプロセスで並んで走る試験が
        // 子を起こした瞬間に fd を継いだ疑いがある (macOS は socket を作って
        // から CLOEXEC を付けるので間がある) が、確かめきれていない。
        assert!(!is_held("127.0.0.1:1").await);
    }

    /// 握っているのが自分なら、自分の pid が返る。`lsof` が無ければ
    /// 「分からない」であって、空 (= 誰も握っていない) とは言わない。
    #[tokio::test]
    async fn the_holder_of_a_port_is_named() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let found = holders(port).await;
        if lsof_is_here() {
            // 並んで走る試験の子が、起きる瞬間に fd を継いで一緒に挙がりうる
            // (上の試験の注記)。自分が挙がっていることだけを見る。
            let found = found.expect("lsof answers");
            assert!(found.contains(&std::process::id()), "{found:?}");
            assert_eq!(holders(1).await, Some(vec![]));
        } else {
            assert_eq!(found, None);
        }
    }

    /// 空いていれば空、塞がっていれば握っている者を並べる。子が居ないのに
    /// 塞がっていれば、誰であれよそ者。
    #[tokio::test]
    async fn a_port_is_inspected_against_the_current_child() {
        let free = inspect("127.0.0.1:1", None).await;
        assert!(!free.held);
        assert_eq!(free.pids, Some(vec![]));
        assert_eq!(free.foreign, Some(false));
        assert_eq!(blocked_by(&free), None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = listener.local_addr().unwrap().to_string();
        let held = inspect(&listen, None).await;
        assert!(held.held);
        assert_eq!(held.foreign, Some(true));
        let said = blocked_by(&held).unwrap();
        assert!(said.starts_with(&format!("{listen} is held by ")), "{said}");

        // 握っているのが今の子なら、よそ者ではない (pid が引けた時だけ言える)。
        let mine = inspect(&listen, Some(std::process::id())).await;
        match mine.pids {
            Some(_) => assert_eq!(mine.foreign, Some(false)),
            None => assert_eq!(mine.foreign, None),
        }
    }

    /// pid が引けなくても、塞がっていることは言う。
    #[test]
    fn a_holder_that_cannot_be_named_is_still_reported() {
        let holder = PortHolder {
            listen: "127.0.0.1:11309".to_owned(),
            held: true,
            pids: None,
            foreign: Some(true),
        };
        let said = blocked_by(&holder).unwrap();
        assert!(said.contains("could not be identified"), "{said}");
        assert!(said.contains("lsof -nP -iTCP:11309 -sTCP:LISTEN"), "{said}");
    }
}
