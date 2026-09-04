//! 起きたことを、見ている人の受け口へ届ける (DR-0012)。
//!
//! [`crate::events`] は「繋いだ人へ流す」形なので、**面が複数あると届かない
//! ことがある**。待ち受けを 2 つ (stable / unstable) 並べて手前で振り分けて
//! いると、見る側が掴んだ 1 つの面のイベントしか見えない。繋ぎ直したときに
//! 待機側を掴むと、何も起きていないように見える。
//!
//! 向きを逆にして gateway から送れば、面がいくつあっても**同じ受け口に
//! 集まる**。見る側は受けるだけでよく、どの面に繋がっているかを気にしない。
//!
//! 送るのは片道。届かなくても送り直さないし、順番も保証しない — 受け口が
//! 落ちている間の知らせは、そもそも見ている人がいない。

use std::path::Path;
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout_at};
use tracing::{info, warn};

use crate::config::Webhook;
use crate::events::Notice;

/// 受け口が受理する 1 通の上限。
const MAX_PAYLOAD: usize = 1024 * 1024;

/// 1 通にまとめるのを待つ時間。
///
/// 1 件ごとに送ると、サブエージェントが一斉に動いたときだけ接続を張り
/// 過ぎる。この長さは**見る側の表示が遅れて見えない範囲**で選ぶ (5 分の
/// 残りを数える画面で、0.2 秒の遅れは目に見えない)。
const WINDOW: Duration = Duration::from_millis(200);

/// 1 回の時間窓で集める件数。
///
/// JSON の大きさは送信前に別途分割する。ここではイベントが続き続けても
/// 一度送信へ進めるよう、件数で区切る。
const MAX_BATCH: usize = 100;

/// 送るのを諦めるまで。
///
/// 受け口が黙っていても、こちらは知らせを配るだけの仕事。長く待つ理由がない。
const TIMEOUT: Duration = Duration::from_secs(3);

/// 受け口へ送り続ける。待ち受けと一緒に走らせる。
///
/// 送り先が設定に無ければ、何もせずに戻る (機能ごと無効)。
pub async fn keep_sending(
    config: Webhook,
    http: reqwest::Client,
    mut watching: tokio::sync::broadcast::Receiver<Notice>,
) {
    let (urls, refused) = config.destinations();
    for reason in refused {
        warn!(%reason, "this webhook destination is unusable; not sending to it");
    }
    if urls.is_empty() {
        return;
    }

    // 合言葉は起動時に 1 回だけ読む。読めなければ**この機能だけ**諦める —
    // 知らせが届かないことは、中継そのものを止める理由にならない。
    let token = match read_token(&config.token_file) {
        Ok(token) => token,
        Err(reason) => {
            warn!(
                file = %config.token_file.display(),
                %reason,
                "cannot read the webhook token; not sending notifications"
            );
            return;
        }
    };
    info!(endpoints = urls.len(), "sending the event to the endpoints");

    // 続けて失敗しているときに、同じ warning を並べない。受け口ごとに数える —
    // 1 つが落ちていることを、他の受け口の記録に混ぜない。
    let mut failing = vec![0u64; urls.len()];

    loop {
        let Some(batch) = collect(&mut watching).await else {
            return;
        };
        let (payloads, oversized) = encode_payloads(&batch);
        if oversized > 0 {
            warn!(oversized, "cannot send an event over 1 MB to a webhook");
        }
        for payload in payloads {
            // 同じ知らせを全部の受け口へ。どれかが落ちていても、他への配達は
            // 止めない (DR-0012)。
            for (url, failing) in urls.iter().zip(&mut failing) {
                match send(&http, url, &token, payload.clone(), TIMEOUT).await {
                    Ok(()) => {
                        if *failing > 0 {
                            info!(%url, failed = *failing, "the webhook endpoint recovered");
                            *failing = 0;
                        }
                    }
                    Err(reason) => {
                        // 最初の 1 回だけ言う。落ちている間は数えるだけにして、
                        // 戻ったときにまとめて報告する。
                        if *failing == 0 {
                            warn!(%url, %reason, "cannot send to the webhook");
                        }
                        *failing += 1;
                    }
                }
            }
        }
    }
}

/// 1 通ぶん集める。流す側が畳まれたら `None`。
///
/// 1 件目は**いつまでも待つ** (何も起きなければ何もしない)。2 件目からは
/// [`WINDOW`] の間だけ待って、来なければそこで区切る。
async fn collect(watching: &mut tokio::sync::broadcast::Receiver<Notice>) -> Option<Vec<Notice>> {
    let mut batch = vec![first(watching).await?];
    let until = Instant::now() + WINDOW;

    while batch.len() < MAX_BATCH {
        match timeout_at(until, watching.recv()).await {
            Ok(Ok(event)) => batch.push(event),
            // 追いつけなかった分は諦める。送り直す仕組みを持たない。
            Ok(Err(RecvError::Lagged(missed))) => {
                warn!(missed, "could not deliver all notifications")
            }
            Ok(Err(RecvError::Closed)) => break,
            // 窓を閉じる。
            Err(_) => break,
        }
    }
    Some(batch)
}

/// 次の 1 件を待つ。流す側が畳まれたら `None`。
async fn first(watching: &mut tokio::sync::broadcast::Receiver<Notice>) -> Option<Notice> {
    loop {
        match watching.recv().await {
            Ok(event) => return Some(event),
            Err(RecvError::Lagged(missed)) => warn!(missed, "could not deliver all notifications"),
            Err(RecvError::Closed) => return None,
        }
    }
}

/// 1 MB を超えない JSON 配列へ分ける。
///
/// 単体で上限を超えるイベントは、その 1 件だけを落とす。
fn encode_payloads(events: &[Notice]) -> (Vec<Vec<u8>>, usize) {
    let mut payloads = Vec::new();
    let mut current = Vec::from(b"[".as_slice());
    let mut count = 0usize;
    let mut oversized = 0usize;

    for event in events {
        let encoded = serde_json::to_vec(event).expect("a notice converts to JSON");
        if encoded.len() + 2 > MAX_PAYLOAD {
            oversized += 1;
            continue;
        }
        let separator = usize::from(count > 0);
        if current.len() + separator + encoded.len() + 1 > MAX_PAYLOAD {
            current.push(b']');
            payloads.push(current);
            current = Vec::from(b"[".as_slice());
            count = 0;
        }
        if count > 0 {
            current.push(b',');
        }
        current.extend_from_slice(&encoded);
        count += 1;
    }
    if count > 0 {
        current.push(b']');
        payloads.push(current);
    }
    (payloads, oversized)
}

/// 1 通送る。受理は 204。
///
/// 合言葉は `Authorization` に載せるだけで、**戻り値にも記録にも出さない**。
async fn send(
    http: &reqwest::Client,
    url: &url::Url,
    token: &str,
    payload: Vec<u8>,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let resp = http
        .post(url.clone())
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(payload)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "no response (timeout)".to_owned()
            } else {
                e.to_string()
            }
        })?;

    let status = resp.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(());
    }
    // 相手の本文は記録しない。受け口が要求内容を反射する実装でも、
    // Authorization の値をログへ持ち込まないため。
    Err(format!("received {status}"))
}

/// 合言葉をファイルから読む。
///
/// **中身をそのまま持ち帰る以外のことをしない** — 記録にも文言にも出さない
/// ので、読めない理由だけを返す。
fn read_token(path: &Path) -> std::result::Result<String, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let token = raw.trim();
    if token.is_empty() {
        return Err("content is empty".to_owned());
    }
    if token.contains(['\r', '\n']) {
        return Err("must be a single line".to_owned());
    }
    reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        "contains characters that cannot be used in an Authorization header".to_owned()
    })?;
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, Events, Notice, Origin};
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_800_000_000;
    const TOKEN: &str = "tok-DO-NOT-LOG-0123456789";

    /// 転送の知らせ 1 件。
    fn event(credential: &str) -> Notice {
        Notice::Request(Event::new(
            NOW,
            &Origin {
                session_id: Some("s-1"),
                prefix: Some("2cf24dba"),
                ns: "personal",
                model: "m",
                credential,
                keepalive: None,
                origin: "main",
                cache_ttl_secs: None,
            },
            200,
        ))
    }

    /// 中身の大きい知らせ 1 件。
    fn bulky(bytes: usize) -> Notice {
        let mut event = Event::new(
            NOW,
            &Origin {
                session_id: Some("s-1"),
                prefix: None,
                ns: "personal",
                model: "m",
                credential: "a",
                keepalive: None,
                origin: "main",
                cache_ttl_secs: None,
            },
            200,
        );
        event.session_id = Some("x".repeat(bytes));
        Notice::Request(event)
    }

    /// 受け口が受け取った 1 件を、転送の知らせとして読む。
    fn request(notice: &Notice) -> &Event {
        notice.request().expect("a forwarding notice")
    }

    /// 受け取った要求を覚えておく受け口。
    struct Receiver {
        url: String,
        seen: Arc<Mutex<Vec<String>>>,
        arrived: Arc<tokio::sync::Semaphore>,
    }

    impl Receiver {
        /// `status` を返し続ける受け口を立てる。
        async fn answering(status: u16) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let arrived = Arc::new(tokio::sync::Semaphore::new(0));

            let recorder = Arc::clone(&seen);
            let bell = Arc::clone(&arrived);
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let recorder = Arc::clone(&recorder);
                    let bell = Arc::clone(&bell);
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                        let mut buf = vec![0u8; 65536];
                        let read = sock.read(&mut buf).await.unwrap_or(0);
                        recorder
                            .lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&buf[..read]).into_owned());

                        let head = format!(
                            "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.flush().await;
                        bell.add_permits(1);
                    });
                }
            });

            Self {
                url: format!("http://{addr}"),
                seen,
                arrived,
            }
        }

        async fn next_delivery(&self) {
            self.arrived.acquire().await.expect("never closes").forget();
        }

        fn deliveries(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    /// 合言葉のファイルを置く。
    fn with_token(dir: &tempfile::TempDir, token: &str) -> std::path::PathBuf {
        let path = dir.path().join("webhook.token");
        std::fs::write(&path, format!("{token}\n")).unwrap();
        path
    }

    fn config(base_url: Option<&str>, token_file: std::path::PathBuf) -> Webhook {
        Webhook {
            base_url: base_url.map(str::to_owned),
            base_urls: Vec::new(),
            token_file,
        }
    }

    /// 受け口を複数書いた設定。
    fn config_for(bases: &[&str], token_file: std::path::PathBuf) -> Webhook {
        Webhook {
            base_url: None,
            base_urls: bases.iter().map(|base| (*base).to_owned()).collect(),
            token_file,
        }
    }

    /// 送り先を書かなければ、何もしない。
    #[tokio::test]
    async fn nothing_is_sent_without_a_destination() {
        let dir = tempfile::TempDir::new().unwrap();
        let receiver = Receiver::answering(204).await;
        let events = Events::new();

        // 送り先が無いので、すぐ戻る (戻らなければここで止まる)。
        keep_sending(
            config(None, with_token(&dir, TOKEN)),
            reqwest::Client::new(),
            events.subscribe(),
        )
        .await;

        events.publish(event("a"));
        assert!(receiver.deliveries().is_empty());
    }

    /// 合言葉が読めなければ、この機能だけ諦める。中継は止めない。
    #[tokio::test]
    async fn an_unreadable_token_disables_only_the_sending() {
        let dir = tempfile::TempDir::new().unwrap();
        let receiver = Receiver::answering(204).await;

        for token_file in [dir.path().join("ない.token"), with_token(&dir, "   ")] {
            let events = Events::new();
            keep_sending(
                config(Some(&receiver.url), token_file),
                reqwest::Client::new(),
                events.subscribe(),
            )
            .await;
        }
        assert!(receiver.deliveries().is_empty());
    }

    /// 起きたことが、受け口へ届く。
    #[tokio::test]
    async fn what_happens_is_delivered() {
        let dir = tempfile::TempDir::new().unwrap();
        let receiver = Receiver::answering(204).await;

        let sending = {
            let url = receiver.url.clone();
            let token_file = with_token(&dir, TOKEN);
            let events = Arc::new(Events::new());
            let running = Arc::clone(&events);
            let task = tokio::spawn(async move {
                keep_sending(
                    config(Some(&url), token_file),
                    reqwest::Client::new(),
                    running.subscribe(),
                )
                .await;
            });
            (events, task)
        };
        let (events, task) = sending;

        // 送る側が見始めるまで待つ。
        while events.watchers() == 0 {
            tokio::task::yield_now().await;
        }
        events.publish(event("claude-kawazzz"));

        receiver.next_delivery().await;
        let delivered = receiver.deliveries().remove(0);

        assert!(
            delivered.starts_with("POST /webhook/llm-gateway"),
            "{delivered}"
        );
        assert!(
            delivered
                .to_lowercase()
                .contains("content-type: application/json"),
            "{delivered}"
        );
        assert!(
            delivered.contains(&format!("authorization: Bearer {TOKEN}"))
                || delivered.contains(&format!("Authorization: Bearer {TOKEN}")),
            "missing Bearer authentication"
        );

        let body = delivered.split("\r\n\r\n").nth(1).expect("body");
        let sent: Vec<Notice> = serde_json::from_str(body).expect("sent as an array");
        assert_eq!(sent.len(), 1);
        assert_eq!(request(&sent[0]).credential, "claude-kawazzz");
        assert_eq!(
            request(&sent[0]).prefix.as_deref(),
            Some("2cf24dba"),
            "same shape as SSE"
        );
        task.abort();
    }

    /// 一度に起きた分は 1 通にまとめる。
    #[tokio::test]
    async fn a_burst_goes_out_as_one_delivery() {
        let dir = tempfile::TempDir::new().unwrap();
        let receiver = Receiver::answering(204).await;
        let events = Arc::new(Events::new());
        let running = Arc::clone(&events);
        let url = receiver.url.clone();
        let token_file = with_token(&dir, TOKEN);
        let task = tokio::spawn(async move {
            keep_sending(
                config(Some(&url), token_file),
                reqwest::Client::new(),
                running.subscribe(),
            )
            .await;
        });

        while events.watchers() == 0 {
            tokio::task::yield_now().await;
        }
        // サブエージェントが一斉に動いたときの形。
        for n in 0..10 {
            events.publish(event(&format!("c{n}")));
        }

        receiver.next_delivery().await;
        let body = receiver.deliveries().remove(0);
        let body = body.split("\r\n\r\n").nth(1).expect("body");
        let sent: Vec<Notice> = serde_json::from_str(body).unwrap();

        assert_eq!(sent.len(), 10, "10 items arrive in a single delivery");
        assert_eq!(
            receiver.deliveries().len(),
            1,
            "only a single connection too"
        );
        task.abort();
    }

    /// 件数内でも大きければ、1 MB 以下の複数通に分ける。
    #[test]
    fn payloads_never_exceed_one_megabyte() {
        let events = vec![bulky(16 * 1024); 100];

        let (payloads, oversized) = encode_payloads(&events);
        assert_eq!(oversized, 0);
        assert!(payloads.len() > 1, "a chunk over 1 MB is split");
        assert!(payloads.iter().all(|payload| payload.len() <= MAX_PAYLOAD));
        let delivered = payloads
            .iter()
            .map(|payload| {
                serde_json::from_slice::<Vec<Notice>>(payload)
                    .unwrap()
                    .len()
            })
            .sum::<usize>();
        assert_eq!(delivered, 100, "no events are dropped even when split");
    }

    /// 1 件だけで上限を超えるものは送らない。
    #[test]
    fn an_oversized_event_is_dropped_alone() {
        let (payloads, oversized) = encode_payloads(&[bulky(MAX_PAYLOAD)]);
        assert!(payloads.is_empty());
        assert_eq!(oversized, 1);
    }

    /// 断られても送り続ける。次の分は普通に送る。
    #[tokio::test]
    async fn a_refused_delivery_does_not_stop_the_sender() {
        for status in [401, 404, 500] {
            let dir = tempfile::TempDir::new().unwrap();
            let receiver = Receiver::answering(status).await;
            let events = Arc::new(Events::new());
            let running = Arc::clone(&events);
            let url = receiver.url.clone();
            let token_file = with_token(&dir, TOKEN);
            let task = tokio::spawn(async move {
                keep_sending(
                    config(Some(&url), token_file),
                    reqwest::Client::new(),
                    running.subscribe(),
                )
                .await;
            });

            while events.watchers() == 0 {
                tokio::task::yield_now().await;
            }
            events.publish(event("a"));
            receiver.next_delivery().await;

            events.publish(event("b"));
            receiver.next_delivery().await;
            assert_eq!(
                receiver.deliveries().len(),
                2,
                "still sends the next one even after {status}"
            );
            task.abort();
        }
    }

    /// 黙った受け口は 3 秒で諦める。
    #[tokio::test]
    async fn a_silent_receiver_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(tokio::sync::Semaphore::new(0));
        let bell = Arc::clone(&accepted);
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            bell.add_permits(1);
            std::future::pending::<()>().await;
        });

        let url = url::Url::parse(&format!("http://{addr}/webhook/llm-gateway")).unwrap();
        let (mut payloads, _) = encode_payloads(&[event("a")]);
        let sending = tokio::spawn(async move {
            send(
                &reqwest::Client::new(),
                &url,
                TOKEN,
                payloads.remove(0),
                Duration::from_millis(20),
            )
            .await
        });
        accepted.acquire().await.unwrap().forget();
        let error = sending.await.unwrap().unwrap_err();
        assert_eq!(error, "no response (timeout)");
    }

    /// 受け口が黙っていても、起きたことを流す側は待たない。
    ///
    /// 送るのは切り離した仕事なので、転送そのものはここに影響されない。
    /// 時間を測ると相手のマシンの都合で揺れるので、**見る側が 1 件も受け取ら
    /// なくても流し込みが戻る**ことで見る。
    #[tokio::test]
    async fn publishing_never_waits_for_the_receiver() {
        let events = Events::new();
        // 受け取らない見物人。溜められる数を超えても、流す側は詰まらない。
        let _stalled = events.subscribe();

        for n in 0..1000 {
            events.publish(event(&format!("c{n}")));
        }
    }

    /// 合言葉を記録に出さない。
    #[test]
    fn the_token_never_reaches_the_logs() {
        let logs = logs_of(|| async {
            // 断る受け口。失敗の記録に合言葉が混ざらないかを見る。
            let receiver = Receiver::answering(401).await;
            let url = url::Url::parse(&format!("{}/webhook/llm-gateway", receiver.url)).unwrap();
            let (mut payloads, _) = encode_payloads(&[event("a")]);
            let reason = send(
                &reqwest::Client::new(),
                &url,
                TOKEN,
                payloads.remove(0),
                TIMEOUT,
            )
            .await
            .unwrap_err();
            warn!(%url, %reason, "cannot send to the webhook");
        });

        assert!(
            !logs.contains(TOKEN),
            "the secret token appears in the logs:\n{logs}"
        );
        assert!(logs.contains("webhook"), "nothing was logged:\n{logs}");
    }

    /// 合言葉のファイルは、前後の空白を落として読む。
    #[test]
    fn the_token_is_read_without_its_newline() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(read_token(&with_token(&dir, TOKEN)).unwrap(), TOKEN);
        assert!(read_token(&dir.path().join("ない")).is_err());
    }

    /// `run` の間に出た記録を集める。受け取り先はこのスレッドにだけ差し込む。
    fn logs_of<F: std::future::Future<Output = ()>>(run: impl FnOnce() -> F) -> String {
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run());
        });
        let text = buffer.0.lock().unwrap().clone();
        String::from_utf8_lossy(&text).into_owned()
    }

    /// 受け口を複数書いたら、同じ知らせが全部へ届く。
    ///
    /// どこへ送るかを選ばないのは、会話の id が世界で一意で、受け取った側が
    /// 自分の知らない会話を捨てるから (DR-0012)。
    #[tokio::test]
    async fn every_destination_gets_the_same_event() {
        let dir = tempfile::TempDir::new().unwrap();
        let one = Receiver::answering(204).await;
        let other = Receiver::answering(204).await;

        let events = Arc::new(Events::new());
        let running = Arc::clone(&events);
        let token_file = with_token(&dir, TOKEN);
        let bases = [one.url.clone(), other.url.clone()];
        let task = tokio::spawn(async move {
            let bases: Vec<&str> = bases.iter().map(String::as_str).collect();
            keep_sending(
                config_for(&bases, token_file),
                reqwest::Client::new(),
                running.subscribe(),
            )
            .await;
        });

        while events.watchers() == 0 {
            tokio::task::yield_now().await;
        }
        events.publish(event("claude-kawazzz"));

        for receiver in [&one, &other] {
            receiver.next_delivery().await;
            let delivered = receiver.deliveries().remove(0);
            assert!(
                delivered.starts_with("POST /webhook/llm-gateway"),
                "{delivered}"
            );
            let body = delivered.split("\r\n\r\n").nth(1).expect("body");
            let sent: Vec<Notice> = serde_json::from_str(body).expect("sent as an array");
            assert_eq!(request(&sent[0]).credential, "claude-kawazzz");
        }
        task.abort();
    }

    /// 1 つの受け口が落ちていても、他へは届く。
    #[tokio::test]
    async fn a_broken_destination_does_not_stop_the_others() {
        let dir = tempfile::TempDir::new().unwrap();
        let broken = Receiver::answering(500).await;
        let healthy = Receiver::answering(204).await;

        let events = Arc::new(Events::new());
        let running = Arc::clone(&events);
        let token_file = with_token(&dir, TOKEN);
        let bases = [broken.url.clone(), healthy.url.clone()];
        let task = tokio::spawn(async move {
            let bases: Vec<&str> = bases.iter().map(String::as_str).collect();
            keep_sending(
                config_for(&bases, token_file),
                reqwest::Client::new(),
                running.subscribe(),
            )
            .await;
        });

        while events.watchers() == 0 {
            tokio::task::yield_now().await;
        }
        events.publish(event("a"));
        healthy.next_delivery().await;
        assert_eq!(healthy.deliveries().len(), 1);

        // 落ちている先を諦めない。次の分も両方へ試す。
        events.publish(event("b"));
        healthy.next_delivery().await;
        assert_eq!(healthy.deliveries().len(), 2);
        assert_eq!(
            broken.deliveries().len(),
            2,
            "the broken one is still tried"
        );
        task.abort();
    }
}
