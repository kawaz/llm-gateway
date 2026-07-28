//! リクエスト 1 本を捌く。
//!
//! モデル名から経路を選び、上から試して、通ったものの応答をそのまま返す。
//! 切り替えられるのは**クライアントへ 1 バイトも書く前**まで。応答を流し
//! 始めた後で upstream が切れても、HTTP のやり直しはできない。

use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};

use crate::backend::anthropic::{Headers, beta, forward, model_of};
use crate::config::{Config, Namespace};
use crate::credential::{CredentialStore, Persistence};
use crate::error::UpstreamAttempt;
use crate::router::{Route, Router};
use crate::session;
use crate::{Error, Result};

pub struct Gateway<P: Persistence> {
    config: Config,
    router: Router,
    credentials: CredentialStore<P>,
    http: reqwest::Client,
    refresh_interval: std::time::Duration,
}

impl<P: Persistence> Gateway<P> {
    pub fn new(config: &Config, persistence: P) -> Result<Self> {
        let http = reqwest::Client::builder()
            // upstream の応答は長い。生成が続く限り待つ必要があるので、
            // 全体のタイムアウトは置かない。接続だけ短く切る。
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Config(format!("HTTP クライアントを作れません: {e}")))?;

        Ok(Self {
            refresh_interval: std::time::Duration::from_secs(config.discovery.refresh_secs),
            router: Router::new(config.clone()),
            config: config.clone(),
            credentials: CredentialStore::new(persistence, http.clone()),
            http,
        })
    }

    /// upstream に一覧を聞く。起動時に 1 回呼ぶ。
    pub async fn refresh_models(&self) {
        self.router.refresh(&self.http, &self.credentials).await;
    }

    /// 一覧を取り直し続ける。
    ///
    /// 新しいモデルが出たときに、再起動せずに拾えるようにする。
    pub async fn keep_models_fresh(&self) {
        let mut ticker = tokio::time::interval(self.refresh_interval);
        // 起動直後の 1 回目は呼び出し側が済ませている。
        ticker.tick().await;
        loop {
            ticker.tick().await;
            self.refresh_models().await;
        }
    }

    /// 名前で namespace を引く。無ければ `None`。
    pub fn namespace(&self, name: &str) -> Option<&Namespace> {
        self.config.namespace(name)
    }

    /// 公開している namespace 名。
    pub fn namespace_names(&self) -> Vec<&str> {
        self.config.namespace_names()
    }

    /// この namespace に見せるモデル名。
    pub async fn models(&self, ns: &Namespace) -> Vec<String> {
        self.router.models(ns).await
    }

    /// このモデルを実際に試す順。設定の優先順のうち、扱える経路だけ。
    pub async fn route_names(&self, ns: &Namespace, model: &str) -> Vec<String> {
        self.router.route_names(ns, model).await
    }

    /// 転送する。
    ///
    /// `path` は `/v1/messages` のようなクライアントが叩いたパス。
    pub async fn forward(
        &self,
        ns: &Namespace,
        path: &str,
        query: Option<&str>,
        mut body: Value,
        headers: Vec<(String, String)>,
    ) -> Result<forward::Response> {
        let requested = model_of(&body)?.to_owned();

        // `opus` のような短い名前は、ここで実際のモデル名に直す。
        // upstream はこの名前を知らないので、ボディも書き換える。
        let model = self.router.resolve(ns, &requested).await;
        if model != requested {
            crate::backend::anthropic::rewrite_model(&mut body, &model);
        }

        let session = session::derive(&body, &headers);
        let routes = self.router.routes_for(ns, &model, &session).await?;

        let mut attempts = Vec::new();
        for route in &routes {
            match self.try_route(route, path, query, &body, &headers).await {
                Ok(resp) => {
                    self.router.remember(&session, &model, route).await;
                    // ここまでで届いているのはヘッダだけ。本文がクライアント
                    // まで流れ切ったかどうかは crate::relay が記録する。
                    info!(
                        model = %model,
                        route = route.name(),
                        status = resp.status,
                        "upstream のヘッダを受け取りました"
                    );
                    return Ok(resp);
                }
                Err(reason) => {
                    warn!(model = %model, route = route.name(), %reason, "経路を切り替えます");
                    attempts.push(UpstreamAttempt {
                        provider: route.name().to_owned(),
                        reason,
                    });
                }
            }
        }

        Err(Error::AllUpstreamsFailed { model, attempts })
    }

    /// 1 経路を試す。切り替える価値のある失敗なら理由を返す。
    ///
    /// 戻り値の `Err` は「次を試してよい」という意味で、呼び出し側へ
    /// そのまま返すべきエラーではない。切り替えても直らない失敗
    /// (認証情報が無い、リクエストが不正) は `Ok` の応答として返し、
    /// クライアントに伝える。
    async fn try_route(
        &self,
        route: &Arc<Route>,
        path: &str,
        query: Option<&str>,
        body: &Value,
        headers: &[(String, String)],
    ) -> std::result::Result<forward::Response, String> {
        let credential = match &route.credential {
            Some(id) => match self.credentials.acquire(id).await {
                Ok(c) => Some(c),
                // 認証情報を用意できないなら、この経路は使えない。
                // 他の経路は別の認証情報を使うので、試す価値がある。
                Err(e) => return Err(e.to_string()),
            },
            None => None,
        };

        // upstream の既定に、この認証情報で拒否されたと分かっている分を足す。
        // 同じ upstream でも region や契約で受け付ける beta が違うので、
        // 学習結果は認証情報ごとに持つ (DR-0003)。
        let mut policy = route.provider.beta_policy();
        if let Some(c) = &credential {
            policy.deny_all(c.denied_beta.iter().cloned());
        }

        let mut sending = Headers::new(headers.to_vec());
        let sent = policy.apply_to(&mut sending);

        let resp = self
            .send(route, credential.as_ref(), path, query, body, sending)
            .await?;

        // beta を載せていないなら、400 の原因は他にある。
        if resp.status != 400 || sent.is_empty() {
            return accept_or_switch(resp);
        }

        let (resp, raw) = forward::buffer(resp).await.map_err(|e| e.to_string())?;
        let raw = String::from_utf8_lossy(&raw);
        if !beta::is_invalid_beta_error(&raw) {
            return accept_or_switch(resp);
        }

        let blamed = beta::blamed_flags(&raw, &sent);
        warn!(
            route = route.name(),
            flags = ?blamed,
            "beta フラグが拒否されました。落として送り直します"
        );
        if let Some(id) = &route.credential
            && let Err(e) = self.credentials.record_denied_beta(id, &blamed).await
        {
            // 覚えられなくても転送は続ける。次も同じ 400 を 1 回踏むだけで、
            // ここで諦めるとクライアントには何も返らない。
            warn!(credential = %id, %e, "拒否された beta フラグを保存できません");
        }

        policy.deny_all(blamed);
        let mut retrying = Headers::new(headers.to_vec());
        policy.apply_to(&mut retrying);

        // 送り直すのは 1 回だけ。これでも 400 ならクライアントへ返す。
        let resp = self
            .send(route, credential.as_ref(), path, query, body, retrying)
            .await?;
        accept_or_switch(resp)
    }

    async fn send(
        &self,
        route: &Arc<Route>,
        credential: Option<&crate::credential::Credential>,
        path: &str,
        query: Option<&str>,
        body: &Value,
        headers: Headers,
    ) -> std::result::Result<forward::Response, String> {
        forward::send(
            &self.http,
            route.provider.as_ref(),
            credential,
            path,
            query,
            body.clone(),
            headers,
        )
        .await
        .map_err(|e| e.to_string())
    }
}

/// この応答をクライアントへ返すか、別の経路を試すか。
fn accept_or_switch(resp: forward::Response) -> std::result::Result<forward::Response, String> {
    if forward::should_try_next(resp.status) {
        return Err(format!("upstream が {} を返しました", resp.status));
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::stored::{OauthTokens, Payload};
    use crate::credential::time::{format_rfc3339, now_unix};
    use crate::credential::{CredentialId, StoredCredential};
    use serde_json::json;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 何度目の要求かで応答を変えられる試験用 upstream。
    ///
    /// 受け取った要求も覚えておく。何を送ったか (どのヘッダが載っていたか) を
    /// 確かめないと、落としたつもりで載せたままの取り違えに気づけない。
    struct FakeUpstream {
        url: String,
        hits: Arc<AtomicUsize>,
        requests: Arc<StdMutex<Vec<String>>>,
    }

    /// discovery の問い合わせに返す一覧。転送の試験と混ぜない。
    const MODELS: &str = r#"{"data":[{"id":"m","created_at":"2026-07-24T00:00:00Z"}]}"#;

    impl FakeUpstream {
        /// `status` を返し続ける。
        async fn always(status: u16) -> Self {
            Self::start(move |_, _| (status, body_for(status))).await
        }

        /// 最初の 1 回だけ `first`、以降 `rest`。
        async fn then(first: u16, rest: u16) -> Self {
            Self::start(move |n, _| {
                let s = if n == 1 { first } else { rest };
                (s, body_for(s))
            })
            .await
        }

        async fn start(
            respond: impl Fn(usize, &str) -> (u16, String) + Send + Sync + 'static,
        ) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let counter = Arc::clone(&hits);
            let seen = Arc::clone(&requests);
            let respond = Arc::new(respond);

            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let counter = Arc::clone(&counter);
                    let seen = Arc::clone(&seen);
                    let respond = Arc::clone(&respond);
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                        let mut buf = vec![0u8; 65536];
                        let read = sock.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..read]).into_owned();

                        // 一覧の問い合わせは数に入れない (転送だけ数える)。
                        let (status, body) = if req.starts_with("GET /v1/models") {
                            (200, MODELS.to_owned())
                        } else {
                            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                            let answer = respond(n, &req);
                            seen.lock().unwrap().push(req);
                            answer
                        };

                        let resp = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.flush().await;
                    });
                }
            });

            Self {
                url: format!("http://{addr}"),
                hits,
                requests,
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        /// 受け取った要求 (ヘッダを含む生のまま)。
        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn body_for(status: u16) -> String {
        if status == 200 {
            r#"{"type":"message","content":[{"type":"text","text":"ok"}]}"#.to_owned()
        } else {
            format!(r#"{{"type":"error","error":{{"message":"status {status}"}}}}"#)
        }
    }

    /// 常に有効な認証情報を返す置き場。保存された内容は覚えておく。
    #[derive(Clone)]
    struct StaticStore(Arc<StdMutex<StoredCredential>>);

    impl StaticStore {
        fn new() -> Self {
            Self::holding(valid_credential())
        }

        fn holding(c: StoredCredential) -> Self {
            Self(Arc::new(StdMutex::new(c)))
        }

        /// 最後に保存された内容。
        fn saved(&self) -> StoredCredential {
            self.0.lock().unwrap().clone()
        }
    }

    fn valid_credential() -> StoredCredential {
        StoredCredential::new(Payload::ClaudeOauth(OauthTokens {
            access_token: "tok".into(),
            refresh_token: "rt".into(),
            // 十分先。更新に入らせない。
            expired: "2099-01-01T00:00:00Z".into(),
            email: "a@b.c".into(),
            extra: Default::default(),
        }))
    }

    impl Persistence for StaticStore {
        fn load(&self, _id: &CredentialId) -> Result<StoredCredential> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn store(&self, _id: &CredentialId, v: &StoredCredential) -> Result<()> {
            *self.0.lock().unwrap() = v.clone();
            Ok(())
        }
        fn list(&self) -> Result<Vec<CredentialId>> {
            Ok(vec![])
        }
    }

    /// discovery を済ませた状態で返す。
    ///
    /// 試験では relay 型を使う。upstream に一覧を聞きに行かず、設定に
    /// 書いたモデルをそのまま扱うので、偽の upstream 1 つで完結する。
    /// 認証情報が要る経路を試すときだけ `claude_oauth` を使う。
    async fn gateway(config_toml: &str) -> Gateway<StaticStore> {
        gateway_with(config_toml, StaticStore::new()).await
    }

    async fn gateway_with(config_toml: &str, store: StaticStore) -> Gateway<StaticStore> {
        let config: Config = toml::from_str(config_toml).unwrap();
        config.validate().unwrap();
        let gw = Gateway::new(&config, store).unwrap();
        gw.refresh_models().await;
        gw
    }

    /// 既定の namespace。
    fn ns<P: Persistence>(gw: &Gateway<P>) -> &Namespace {
        gw.namespace(crate::config::DEFAULT_NAMESPACE)
            .expect("既定は必ずある")
    }

    fn request() -> Value {
        json!({
            "model": "m",
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": r#"{"session_id":"s1"}"#},
        })
    }

    async fn body_text(resp: forward::Response) -> String {
        String::from_utf8(forward::collect_body(resp.body).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn forwards_to_the_first_route() {
        let up = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#,
            up.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        assert!(body_text(resp).await.contains("ok"));
        assert_eq!(up.hits(), 1);
    }

    /// 経路が断たれていたら次を試す。
    #[tokio::test]
    async fn falls_back_when_upstream_is_down() {
        let down = FakeUpstream::always(503).await;
        let alive = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.down]
type = "relay"
url = "{}"
models = ["m"]

[credentials.alive]
type = "relay"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["down", "alive"]
"#,
            down.url, alive.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(down.hits(), 1, "先に試す");
        assert_eq!(alive.hits(), 1, "落ちていたので次へ");
    }

    /// 繋がらない先も飛ばす。
    #[tokio::test]
    async fn falls_back_when_upstream_is_unreachable() {
        let alive = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.nowhere]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m"]

[credentials.alive]
type = "relay"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["nowhere", "alive"]
"#,
            alive.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
    }

    /// リクエスト側の誤りは、そのままクライアントへ返す。
    /// 経路を替えても直らないので、他を試すのは無駄。
    #[tokio::test]
    async fn client_error_is_returned_without_retry() {
        let up = FakeUpstream::always(400).await;
        let other = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{}"
models = ["m"]

[credentials.b]
type = "relay"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a", "b"]
"#,
            up.url, other.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.status, 400);
        assert_eq!(other.hits(), 0, "次を試さない");
    }

    /// レート制限でも切り替えない。別の経路でも同じように当たる。
    #[tokio::test]
    async fn rate_limit_is_returned_without_retry() {
        let up = FakeUpstream::always(429).await;
        let other = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{}"
models = ["m"]

[credentials.b]
type = "relay"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a", "b"]
"#,
            up.url, other.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!(resp.status, 429);
        assert_eq!(other.hits(), 0);
    }

    /// 全部落ちていたら、どこで何が起きたかを添えて返す。
    #[tokio::test]
    async fn reports_every_attempt_when_all_fail() {
        let a = FakeUpstream::always(503).await;
        let b = FakeUpstream::always(502).await;
        let gw = gateway(&format!(
            r#"
[credentials.first]
type = "relay"
url = "{}"
models = ["m"]

[credentials.second]
type = "relay"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["first", "second"]
"#,
            a.url, b.url
        ))
        .await;

        let err = gw
            .forward(ns(&gw), "/v1/messages", None, request(), vec![])
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains('m'), "どのモデルか: {msg}");
        assert!(msg.contains('2'), "何件試したか: {msg}");

        let Error::AllUpstreamsFailed { attempts, .. } = err else {
            panic!("全経路失敗のはず");
        };
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].provider, "first");
        assert!(attempts[0].reason.contains("503"), "{:?}", attempts[0]);
        assert!(attempts[1].reason.contains("502"), "{:?}", attempts[1]);
    }

    /// 一度通った経路を次も先に試す。
    #[tokio::test]
    async fn remembers_the_route_that_worked() {
        let flaky = FakeUpstream::then(503, 200).await;
        let alive = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.flaky]
type = "relay"
url = "{}"
models = ["m"]

[credentials.alive]
type = "relay"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["flaky", "alive"]
"#,
            flaky.url, alive.url
        ))
        .await;

        // 1 回目: flaky が落ちていて alive が通る。
        gw.forward(ns(&gw), "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!((flaky.hits(), alive.hits()), (1, 1));

        // 2 回目: flaky は復帰しているが、通った alive を先に試す。
        gw.forward(ns(&gw), "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!(
            (flaky.hits(), alive.hits()),
            (1, 2),
            "復帰した先より、通った先を優先する"
        );
    }

    #[tokio::test]
    async fn unknown_model_is_rejected() {
        let gw = gateway(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m"]

[[ns.default.routing]]
models = ["known"]
credentials = ["a"]
"#,
        )
        .await;
        let err = gw
            .forward(
                ns(&gw),
                "/v1/messages",
                None,
                json!({"model": "unknown"}),
                vec![],
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[tokio::test]
    async fn missing_model_is_rejected() {
        let gw = gateway(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#,
        )
        .await;
        assert!(
            gw.forward(
                ns(&gw),
                "/v1/messages",
                None,
                json!({"max_tokens": 1}),
                vec![]
            )
            .await
            .is_err()
        );
    }

    /// 公開する一覧は credential が扱えるものから作る。
    #[tokio::test]
    async fn lists_models_from_credentials() {
        let gw = gateway(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["z-model", "a-model"]

[ns.default]
"#,
        )
        .await;
        assert_eq!(
            gw.models(ns(&gw)).await,
            vec!["a-model", "z-model"],
            "名前順に並ぶ"
        );
    }

    /// 隠したモデルは一覧にも出ないし、転送もしない。
    #[tokio::test]
    async fn excluded_models_are_hidden() {
        let gw = gateway(
            r#"
[ns.default.filter]
exclude = ["claude-opus-4*"]

[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["claude-opus-5", "claude-opus-4-8"]

[ns.default.aliases]
opus = "claude-opus-*"
"#,
        )
        .await;
        assert_eq!(
            gw.models(ns(&gw)).await,
            vec!["claude-opus-5", "opus"],
            "隠した 4-8 は出ない。opus はエイリアス"
        );

        let err = gw
            .forward(
                ns(&gw),
                "/v1/messages",
                None,
                json!({"model": "claude-opus-4-8"}),
                vec![],
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("claude-opus-4-8"), "{err}");
    }

    /// Claude Code 2.1.220 が実際に送る beta の束。
    const CLIENT_BETA: &str = "oauth-2025-04-20,claude-code-20250219,\
advisor-tool-2026-03-01";

    fn beta_header() -> Vec<(String, String)> {
        vec![("anthropic-beta".to_owned(), CLIENT_BETA.to_owned())]
    }

    /// 認証情報を要する経路 1 本だけの設定。
    fn oauth_config(url: &str) -> String {
        format!(
            r#"
[credentials.a]
type = "claude_oauth"
url = "{url}"

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#
        )
    }

    /// beta が原因の 400 は、フラグを落として 1 回だけ送り直す (DR-0003)。
    ///
    /// 拒否された顔ぶれは認証情報に書き戻す。覚えないと毎回 400 を踏む。
    #[tokio::test]
    async fn rejected_beta_is_dropped_learned_and_retried() {
        let up = FakeUpstream::start(|n, _| match n {
            1 => (
                400,
                r#"{"type":"error","error":{"message":"invalid beta flag"}}"#.to_owned(),
            ),
            _ => (200, body_for(200)),
        })
        .await;
        let store = StaticStore::new();
        let gw = gateway_with(&oauth_config(&up.url), store.clone()).await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert_eq!(resp.status, 200, "落として送り直した結果が返る");
        assert_eq!(up.hits(), 2, "送り直すのは 1 回だけ");

        let sent = up.requests();
        assert!(
            sent[0].contains("anthropic-beta"),
            "1 本目はそのまま送る: {}",
            sent[0]
        );
        assert!(
            !sent[1].to_lowercase().contains("anthropic-beta"),
            "2 本目は落として送る: {}",
            sent[1]
        );

        let learned = store.saved();
        for flag in CLIENT_BETA.split(',') {
            assert!(
                learned.denied_beta.contains_key(flag),
                "名前が分からないので送った分を覚える: {:?}",
                learned.denied_beta
            );
        }

        // 覚えた分は、同じ工程の次の転送から効く (毎回 400 を踏まない)。
        gw.forward(ns(&gw), "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();
        assert_eq!(up.hits(), 3, "2 度目は 1 本で済む");
        assert!(
            !up.requests()[2].to_lowercase().contains("anthropic-beta"),
            "覚えた分を載せ直さない: {}",
            up.requests()[2]
        );
    }

    /// 覚えたフラグは、次から最初の 1 本目で落ちる。
    #[tokio::test]
    async fn learned_beta_is_dropped_before_sending() {
        let up = FakeUpstream::always(200).await;
        let mut known = valid_credential();
        known.record_denied_beta(&["advisor-tool-2026-03-01".to_owned()], now_unix());
        let store = StaticStore::holding(known);
        let gw = gateway_with(&oauth_config(&up.url), store).await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(up.hits(), 1, "400 を踏まずに済む");

        let sent = &up.requests()[0];
        assert!(
            sent.contains("oauth-2025-04-20") && sent.contains("claude-code-20250219"),
            "拒否されていない分は残す: {sent}"
        );
        assert!(
            !sent.contains("advisor-tool-2026-03-01"),
            "覚えた分だけ落とす: {sent}"
        );
    }

    /// 記録の期限が切れていたら、また試してみる。
    ///
    /// upstream が対応したときに戻れないと、新機能を取りこぼし続ける。
    #[tokio::test]
    async fn expired_denial_is_tried_again() {
        let up = FakeUpstream::always(200).await;
        let mut stale = valid_credential();
        stale.record_denied_beta(
            &["advisor-tool-2026-03-01".to_owned()],
            now_unix() - 86_400 * 2,
        );
        let store = StaticStore::holding(stale);
        let gw = gateway_with(&oauth_config(&up.url), store).await;

        gw.forward(ns(&gw), "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert!(
            up.requests()[0].contains("advisor-tool-2026-03-01"),
            "24 時間経ったものは通してみる: {}",
            up.requests()[0]
        );
    }

    /// beta と関係ない 400 は、そのままクライアントへ返す。
    #[tokio::test]
    async fn unrelated_client_error_is_not_retried() {
        let up = FakeUpstream::always(400).await;
        let store = StaticStore::new();
        let gw = gateway_with(&oauth_config(&up.url), store.clone()).await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert_eq!(resp.status, 400);
        assert!(
            body_text(resp).await.contains("status 400"),
            "本文を読んだ後もそのまま返す"
        );
        assert_eq!(up.hits(), 1, "送り直さない");
        assert!(
            store.saved().denied_beta.is_empty(),
            "beta のせいでないなら覚えない"
        );
    }

    /// 名指しされた場合は、そのフラグだけを覚える。
    #[tokio::test]
    async fn only_the_named_flag_is_remembered() {
        let up = FakeUpstream::start(|n, _| match n {
            1 => (
                400,
                r#"{"error":{"message":"unsupported beta: advisor-tool-2026-03-01"}}"#.to_owned(),
            ),
            _ => (200, body_for(200)),
        })
        .await;
        let store = StaticStore::new();
        let gw = gateway_with(&oauth_config(&up.url), store.clone()).await;

        let resp = gw
            .forward(ns(&gw), "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();
        assert_eq!(resp.status, 200);

        let learned = store.saved();
        assert_eq!(
            learned.denied_beta.keys().collect::<Vec<_>>(),
            vec!["advisor-tool-2026-03-01"],
            "通るフラグを巻き添えにしない"
        );
        assert!(
            up.requests()[1].contains("claude-code-20250219"),
            "残りは載せたまま送り直す: {}",
            up.requests()[1]
        );
    }

    /// 覚えた時刻は書き戻したものが読み直せる (往復で欠けない)。
    #[tokio::test]
    async fn learned_denial_survives_a_round_trip() {
        let up = FakeUpstream::start(|n, _| match n {
            1 => (400, r#"{"error":"invalid beta flag"}"#.to_owned()),
            _ => (200, body_for(200)),
        })
        .await;
        let store = StaticStore::new();
        let gw = gateway_with(&oauth_config(&up.url), store.clone()).await;

        gw.forward(ns(&gw), "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        let json = serde_json::to_string(&store.saved()).unwrap();
        let reloaded: StoredCredential = serde_json::from_str(&json).unwrap();
        assert_eq!(
            reloaded.denied_beta_at(now_unix()).len(),
            3,
            "読み直しても落とす対象のまま: {json}"
        );
        assert!(
            reloaded
                .denied_beta
                .values()
                .all(|t| t == &format_rfc3339(crate::credential::time::parse_rfc3339(t).unwrap())),
            "時刻は RFC 3339 で書く: {json}"
        );
    }

    /// 短い名前で指定できる。upstream には実際のモデル名で送る。
    #[tokio::test]
    async fn alias_is_resolved_before_forwarding() {
        let up = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{}"
models = ["claude-opus-5"]

[ns.default.aliases]
opus = "claude-opus-*"
"#,
            up.url
        ))
        .await;

        assert!(
            gw.models(ns(&gw)).await.contains(&"opus".to_owned()),
            "一覧に短い名前も出る"
        );

        let resp = gw
            .forward(
                ns(&gw),
                "/v1/messages",
                None,
                json!({"model": "opus"}),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(up.hits(), 1);
    }
}
