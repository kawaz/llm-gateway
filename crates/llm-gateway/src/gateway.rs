//! リクエスト 1 本を捌く。
//!
//! モデル名から経路を選び、上から試して、通ったものの応答をそのまま返す。
//! 切り替えられるのは**クライアントへ 1 バイトも書く前**まで。応答を流し
//! 始めた後で upstream が切れても、HTTP のやり直しはできない。

use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};

use crate::backend::anthropic::{Headers, forward, model_of};
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
                    info!(
                        model = %model,
                        route = route.name(),
                        status = resp.status,
                        "転送しました"
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

        let resp = forward::send(
            &self.http,
            route.provider.as_ref(),
            credential.as_ref(),
            path,
            query,
            body.clone(),
            Headers::new(headers.to_vec()),
        )
        .await
        .map_err(|e| e.to_string())?;

        if forward::should_try_next(resp.status) {
            return Err(format!("upstream が {} を返しました", resp.status));
        }

        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::{CredentialId, Kind, StoredCredential};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 何度目の要求かで応答を変えられる試験用 upstream。
    struct FakeUpstream {
        url: String,
        hits: Arc<AtomicUsize>,
    }

    impl FakeUpstream {
        /// `status` を返し続ける。
        async fn always(status: u16) -> Self {
            Self::start(move |_| (status, body_for(status))).await
        }

        /// 最初の 1 回だけ `first`、以降 `rest`。
        async fn then(first: u16, rest: u16) -> Self {
            Self::start(move |n| {
                let s = if n == 1 { first } else { rest };
                (s, body_for(s))
            })
            .await
        }

        async fn start(respond: impl Fn(usize) -> (u16, String) + Send + Sync + 'static) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&hits);
            let respond = Arc::new(respond);

            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let counter = Arc::clone(&counter);
                    let respond = Arc::clone(&respond);
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                        let mut buf = vec![0u8; 65536];
                        let _ = sock.read(&mut buf).await;
                        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                        let (status, body) = respond(n);
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
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    fn body_for(status: u16) -> String {
        if status == 200 {
            r#"{"type":"message","content":[{"type":"text","text":"ok"}]}"#.to_owned()
        } else {
            format!(r#"{{"type":"error","error":{{"message":"status {status}"}}}}"#)
        }
    }

    /// 常に有効な認証情報を返す置き場。
    struct StaticStore(StdMutex<StoredCredential>);

    impl StaticStore {
        fn new() -> Self {
            Self(StdMutex::new(StoredCredential {
                kind: Kind::Claude,
                email: "a@b.c".into(),
                access_token: "tok".into(),
                refresh_token: "rt".into(),
                // 十分先。更新に入らせない。
                expired: "2099-01-01T00:00:00Z".into(),
                last_refresh: String::new(),
                priority: 0,
                disabled: false,
                excluded_models: vec![],
                account_id: None,
                extra: BTreeMap::new(),
            }))
        }
    }

    impl Persistence for StaticStore {
        fn load(&self, _id: &CredentialId) -> Result<StoredCredential> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn store(&self, _id: &CredentialId, _v: &StoredCredential) -> Result<()> {
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
    async fn gateway(config_toml: &str) -> Gateway<StaticStore> {
        let config: Config = toml::from_str(config_toml).unwrap();
        config.validate().unwrap();
        let gw = Gateway::new(&config, StaticStore::new()).unwrap();
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

[[routing]]
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

[[routing]]
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

[[routing]]
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

[[routing]]
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

[[routing]]
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

[[routing]]
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

[[routing]]
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

[[routing]]
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

[[routing]]
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
[filter]
exclude = ["claude-opus-4*"]

[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["claude-opus-5", "claude-opus-4-8"]

[aliases]
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

[aliases]
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
