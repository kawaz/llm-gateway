//! リクエスト 1 本を捌く。
//!
//! モデル名から経路を選び、上から試して、通ったものの応答をそのまま返す。
//! 切り替えられるのは**クライアントへ 1 バイトも書く前**まで。応答を流し
//! 始めた後で upstream が切れても、HTTP のやり直しはできない。

use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};

use crate::backend::anthropic::{Headers, Official, Provider, beta, forward, model_of};
use crate::config::{Config, CredentialSpec, Namespace};
use crate::credential::time::now_unix;
use crate::credential::{CredentialId, CredentialStore, Persistence};
use crate::error::UpstreamAttempt;
use crate::router::{Route, Router};
use crate::session;
use crate::usage::{self, Usage};
use crate::{Error, Result};

/// 能動プローブに使うモデル。
///
/// ヘッダを得るには実リクエストが要る (副作用ゼロで usage だけ返す口は
/// 見つかっていない、DR-0007)。一番小さいモデルに `max_tokens = 1` で
/// 投げて、消費を最小にする。
const PROBE_MODEL: &str = "claude-haiku-4-5-20251001";

pub struct Gateway<P: Persistence> {
    config: Config,
    router: Router,
    credentials: CredentialStore<P>,
    http: reqwest::Client,
    refresh_interval: std::time::Duration,
    usage: Usage,
}

impl<P: Persistence> Gateway<P> {
    pub fn new(config: &Config, persistence: P) -> Result<Self> {
        let http = reqwest::Client::builder()
            // upstream の応答は長い。生成が続く限り待つ必要があるので、
            // 全体のタイムアウトは置かない。接続だけ短く切る。
            .connect_timeout(std::time::Duration::from_secs(10))
            // Design rationale: keepalive をライブラリ既定 (無効) に任せない。
            // 全体のタイムアウトを置かない以上、コネクションが生きているか
            // どうかは誰かが確かめないと、経路上の NAT/LB に黙って切られた
            // ことに気づくのが「次に何か流そうとした時」まで遅れる。生成の
            // 待ち時間と区別が付かないので、待ち続けたまま止まる。
            //
            // 20 秒に置くのは、よくあるアイドル切断 (30〜60 秒級) より十分
            // 手前で叩いて、経路上の対応表を保たせたいため。握っている
            // コネクションは経路の本数ぶんしかないので、この間隔でも
            // 流れるパケットは無視できる。
            .tcp_keepalive(std::time::Duration::from_secs(20))
            // 応答が無い側は 10 秒おきに 3 回まで。既定 (macOS なら 75 秒 ×
            // 8 回) だと死んだ接続を掴んだまま 10 分粘ることになる。
            .tcp_keepalive_interval(std::time::Duration::from_secs(10))
            .tcp_keepalive_retries(3)
            // upstream (api.anthropic.com / bedrock) とは ALPN で h2 に
            // なる。TCP の keepalive は TLS の下を通るので、中身のバイト数
            // でアイドルを測る類の LB からは「無音」のままに見える。h2 の
            // PING は TLS の上に乗るので、そちらにも生きていると伝わり、
            // かつ TCP は生きたまま h2 が応じない状態も掴める。
            .http2_keep_alive_interval(std::time::Duration::from_secs(20))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
            // 転送していない間も打つ。使い回し待ちで寝ている接続が黙って
            // 死んでいると、次の転送がそれを掴んで落ち、経路を切り替えた
            // 扱いになって別の認証情報へ流れてしまう。
            .http2_keep_alive_while_idle(true)
            .build()
            .map_err(|e| Error::Config(format!("HTTP クライアントを作れません: {e}")))?;

        Ok(Self {
            refresh_interval: std::time::Duration::from_secs(config.discovery.refresh_secs),
            router: Router::new(config.clone()),
            config: config.clone(),
            credentials: CredentialStore::new(persistence, http.clone()),
            http,
            usage: Usage::default(),
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
        ns_name: &str,
        path: &str,
        query: Option<&str>,
        mut body: Value,
        headers: Vec<(String, String)>,
    ) -> Result<Forwarded> {
        let requested = model_of(&body)?.to_owned();

        // `opus` のような短い名前は、ここで実際のモデル名に直す。
        // upstream はこの名前を知らないので、ボディも書き換える。
        let model = self.router.resolve(ns, &requested).await;
        if model != requested {
            crate::backend::anthropic::rewrite_model(&mut body, &model);
        }

        let session = session::derive(&body, &headers);
        let routes = self
            .router
            .routes_for(ns, ns_name, &model, &session)
            .await?;

        let mut attempts = Vec::new();
        // この経路に断られた応答のうち、最後のもの。全滅したときに返す。
        // 出した経路の認証情報を添えるのは、断られた応答も使用量の集計対象
        // (どの credential が断られたか) になるため。
        let mut denied: Option<(forward::Response, Option<CredentialId>)> = None;
        for route in &routes {
            match self.try_route(route, path, query, &body, &headers).await {
                Ok(resp) => {
                    // 貼り付けるのは通った経路だけ。断られた先を覚えると、
                    // 次の転送も同じところから始めることになる。
                    if resp.status / 100 == 2 {
                        self.router.remember(ns_name, &session, &model, route).await;
                    }
                    // ここまでで届いているのはヘッダだけ。本文がクライアント
                    // まで流れ切ったかどうかは crate::relay が記録する。
                    info!(
                        model = %model,
                        route = route.name(),
                        status = resp.status,
                        "upstream のヘッダを受け取りました"
                    );
                    return Ok(Forwarded {
                        response: resp,
                        credential: route.credential.clone(),
                        model,
                    });
                }
                Err(Switch { reason, denial }) => {
                    warn!(model = %model, route = route.name(), %reason, "経路を切り替えます");
                    attempts.push(UpstreamAttempt {
                        provider: route.name().to_owned(),
                        reason,
                    });
                    if let Some(resp) = denial {
                        denied = Some((resp, route.credential.clone()));
                    }
                }
            }
        }

        // 断られた応答を見ていたなら、最後のものをそのまま返す。こちらで
        // 別の状態に置き換えると、`retry-after` のようなクライアントが次の
        // 一手を決める手掛かりまで消える。
        if let Some((resp, credential)) = denied {
            warn!(
                model = %model,
                status = resp.status,
                routes = routes.len(),
                "経路を使い切りました。最後に断られた応答をそのまま返します"
            );
            return Ok(Forwarded {
                response: resp,
                credential,
                model,
            });
        }

        Err(Error::AllUpstreamsFailed { model, attempts })
    }

    /// 1 経路を試す。切り替える価値のある失敗なら理由を返す。
    ///
    /// 戻り値の `Err` は「次を試してよい」という意味で、呼び出し側へ
    /// そのまま返すべきエラーではない。切り替えても直らない失敗
    /// (リクエストが不正) は `Ok` の応答として返し、クライアントに伝える。
    async fn try_route(
        &self,
        route: &Arc<Route>,
        path: &str,
        query: Option<&str>,
        body: &Value,
        headers: &[(String, String)],
    ) -> std::result::Result<forward::Response, Switch> {
        let credential = match &route.credential {
            Some(id) => match self.credentials.acquire(id).await {
                Ok(c) => Some(c),
                // 認証情報を用意できないなら、この経路は使えない。
                // 他の経路は別の認証情報を使うので、試す価値がある。
                Err(e) => return Err(Switch::to_next(e.to_string())),
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

        let (resp, raw) = forward::buffer(resp)
            .await
            .map_err(|e| Switch::to_next(e.to_string()))?;
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
    ) -> std::result::Result<forward::Response, Switch> {
        let resp = forward::send(
            &self.http,
            route.provider.as_ref(),
            credential,
            path,
            query,
            body.clone(),
            headers,
        )
        .await
        .map_err(|e| Switch::to_next(e.to_string()))?;

        // 便乗して利用状況を拾う (DR-0007)。読むのはヘッダだけなので、
        // 本文はこの後もそのまま流れる。上限に当たった応答こそ見たいので、
        // status では絞らない。
        if let Some(id) = &route.credential {
            self.usage.observe(id, &resp.headers, now_unix()).await;
        }
        Ok(resp)
    }

    /// credential ごとの利用状況。
    ///
    /// `probe` が真なら、先に能動プローブを投げてから作る。既定を便乗のみに
    /// するのは、usage の確認が usage を勝手に消費する構図を避けるため
    /// (DR-0007)。
    pub async fn usage_report(&self, probe: bool) -> usage::Report {
        let probed = if probe {
            self.probe_usage().await
        } else {
            None
        };

        let mut credentials = Vec::new();
        for (name, spec) in &self.config.credentials {
            let id = CredentialId::new(name.as_str());
            let snapshot = self.usage.get(&id).await;
            let support = support_of(spec, snapshot.is_some());

            let mut entry = usage::CredentialUsage::new(name, spec.type_name(), support, snapshot);
            entry.probe_error = probed
                .as_ref()
                .and_then(|p| p.errors.get(name.as_str()).cloned());
            credentials.push(entry);
        }

        let mut report = usage::Report::new(now_unix(), credentials);
        report.probe = probed.map(|p| p.spent);
        report
    }

    /// 使用率を取れる credential に、最小のリクエストを 1 本ずつ投げる。
    ///
    /// 失敗した credential はその理由を控えて先へ進む。1 つの認証切れで
    /// 一覧全体が返らなくなると、確認したかった他の credential まで見えない。
    async fn probe_usage(&self) -> Option<Probed> {
        let mut spent = usage::Probe {
            model: PROBE_MODEL.to_owned(),
            ..usage::Probe::default()
        };
        let mut errors = std::collections::BTreeMap::new();

        for (name, spec) in &self.config.credentials {
            // ヘッダを返すのは Anthropic のサブスクだけ (DR-0007)。
            if !matches!(spec, CredentialSpec::ClaudeOauth { .. }) {
                continue;
            }
            spent.requests += 1;
            match self.probe_one(name, spec).await {
                Ok((input, output)) => {
                    spent.input_tokens += input;
                    spent.output_tokens += output;
                }
                Err(reason) => {
                    warn!(credential = %name, %reason, "利用状況を取りに行けません");
                    errors.insert(name.clone(), reason);
                }
            }
        }
        Some(Probed { spent, errors })
    }

    /// 1 つの credential に投げて、ヘッダを拾う。返すのは消費したトークン。
    async fn probe_one(
        &self,
        name: &str,
        spec: &CredentialSpec,
    ) -> std::result::Result<(u64, u64), String> {
        let id = CredentialId::new(name);
        let credential = self
            .credentials
            .acquire(&id)
            .await
            .map_err(|e| e.to_string())?;
        let provider = Official::new(name, spec.url(), spec.headers().clone());

        let body = serde_json::json!({
            "model": PROBE_MODEL,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "."}],
        });
        let headers = Headers::new(vec![
            ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
            ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
        ]);

        let resp = forward::send(
            &self.http,
            &provider as &dyn Provider,
            Some(&credential),
            "/v1/messages",
            None,
            body,
            headers,
        )
        .await
        .map_err(|e| e.to_string())?;

        // 上限に当たった応答にも使用率は載る。状態を見る前に拾っておく。
        let status = resp.status;
        self.usage.observe(&id, &resp.headers, now_unix()).await;

        let raw = forward::collect_body(resp.body)
            .await
            .map_err(|e| e.to_string())?;
        if status != 200 {
            return Err(format!(
                "upstream が {status} を返しました: {}",
                String::from_utf8_lossy(&raw)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ));
        }

        let spent: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let count = |key: &str| {
            spent
                .pointer(&format!("/usage/{key}"))
                .and_then(Value::as_u64)
        };
        Ok((
            count("input_tokens").unwrap_or(0),
            count("output_tokens").unwrap_or(0),
        ))
    }
}

/// 転送した結果。応答と、それを出した経路の身元。
///
/// 身元を応答と別に持つのは、使用量の集計が「どの credential のどのモデルか」で
/// 束ねるのに対し、[`forward::Response`] は HTTP の応答そのもの (状態・ヘッダ・
/// 本文) を表すため。集計の都合を応答の型に混ぜると、転送に関係のない項目が
/// upstream の応答を表す構造体に溜まっていく (DR-0011)。
#[derive(Debug)]
pub struct Forwarded {
    pub response: forward::Response,
    /// この応答を出した credential。relay 型のように認証情報を持たない経路は
    /// `None`。
    pub credential: Option<CredentialId>,
    /// 解決後の実モデル名。短い名前 (`opus`) はここでは解決済み。
    pub model: String,
}

/// プローブの結果。消費した分と、credential ごとの失敗。
struct Probed {
    spent: usage::Probe,
    errors: std::collections::BTreeMap<String, String>,
}

/// この credential の利用状況をどこまで出せるか。
fn support_of(spec: &CredentialSpec, observed: bool) -> usage::Support {
    if observed {
        return usage::Support::Observed;
    }
    match spec {
        CredentialSpec::ClaudeOauth { .. } => usage::Support::Unobserved,
        // 使用量は別の IAM アクションで、実行権限しかない API キーでは取れない。
        CredentialSpec::ClaudeBedrock { .. } => usage::Support::NotApplicable,
        // Codex は転送で凌いでいる段階。転送先が返さないものは見えない。
        CredentialSpec::CodexOauth { .. } | CredentialSpec::Relay { .. } => {
            usage::Support::UpstreamDependent
        }
    }
}

/// 次の経路へ回す理由。
struct Switch {
    reason: String,
    /// この経路に断られた応答。次の経路が全滅したときにクライアントへ返す。
    ///
    /// 本文は読まずに抱えたまま持ち回る。断られた応答は小さいので、後続を
    /// 試している間コネクションを握っていても割に合う。
    denial: Option<forward::Response>,
}

impl Switch {
    /// 応答を伴わない切り替え (経路断・送信できなかった等)。
    fn to_next(reason: String) -> Self {
        Self {
            reason,
            denial: None,
        }
    }
}

/// この応答をクライアントへ返すか、別の経路を試すか。
fn accept_or_switch(resp: forward::Response) -> std::result::Result<forward::Response, Switch> {
    let reason = format!("upstream returned {}", resp.status);
    if forward::should_try_next(resp.status) {
        return Err(Switch::to_next(reason));
    }
    if forward::is_route_denial(resp.status) {
        return Err(Switch {
            reason,
            denial: Some(resp),
        });
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
            Self::start_with_headers(&[], respond).await
        }

        /// 応答に毎回載せるヘッダを添えて立てる。
        ///
        /// `retry-after` のように、状態コードだけでは伝わらないものが
        /// クライアントまで残るかを確かめるのに使う。
        async fn start_with_headers(
            extra: &[(&str, &str)],
            respond: impl Fn(usize, &str) -> (u16, String) + Send + Sync + 'static,
        ) -> Self {
            let extra: Arc<String> =
                Arc::new(extra.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect());
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
                    let extra = Arc::clone(&extra);
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
content-length: {}\r\n{extra}connection: close\r\n\r\n{body}",
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
        /// 置き場を共有する相手がいないので、締め出すものが無い。
        type Guard = ();

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
        fn lock(&self, _id: &CredentialId) -> Result<Self::Guard> {
            Ok(())
        }
        /// 版を持たない。書き換えるのは自分だけなので、控えを疑う理由が無い。
        fn version(&self, _id: &CredentialId) -> Option<u64> {
            None
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

    /// 転送の試験で使う namespace 名。
    const NS: &str = crate::config::DEFAULT_NAMESPACE;

    /// 既定の namespace。
    fn ns<P: Persistence>(gw: &Gateway<P>) -> &Namespace {
        gw.namespace(NS).expect("既定は必ずある")
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
        assert!(body_text(resp.response).await.contains("ok"));
        assert_eq!(up.hits(), 1);
    }

    /// 通った経路の認証情報が応答に付いてくる。使用量をこの鍵で束ねる。
    #[tokio::test]
    async fn a_forwarded_response_names_the_credential_that_answered() {
        let up = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "claude_oauth"
url = "{}"

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#,
            up.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
        assert_eq!(
            resp.credential.as_ref().map(CredentialId::as_str),
            Some("a"),
            "どの認証情報が答えたか"
        );
    }

    /// モデル名は**解決後**のものが付く。
    ///
    /// 短い名前のまま集計すると、同じモデルが別名の数だけ行に分かれる。
    #[tokio::test]
    async fn a_forwarded_response_carries_the_resolved_model() {
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

        let resp = gw
            .forward(
                ns(&gw),
                NS,
                "/v1/messages",
                None,
                json!({"model": "opus", "max_tokens": 8, "messages": []}),
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
        assert_eq!(
            resp.model, "claude-opus-5",
            "短い名前ではなく解決後のモデル名"
        );
    }

    /// 認証情報を持たない経路 (relay) は `None`。集計側で「持ち主なし」に振る。
    #[tokio::test]
    async fn a_route_without_a_credential_has_no_owner() {
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.credential, None);
        assert_eq!(resp.model, "m");
    }

    /// 断られた応答を透過するときも、断った経路の身元が付く。
    ///
    /// 上限に当たった分を集計から落とすと、「使い切った日」が記録に残らない。
    #[tokio::test]
    async fn a_passed_through_denial_still_names_the_route() {
        let denying = FakeUpstream::always(429).await;
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
            denying.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429);
        assert_eq!(resp.model, "m", "断られた応答にもモデル名は付く");
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!(resp.response.status, 200);
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 400);
        assert_eq!(other.hits(), 0, "次を試さない");
    }

    /// 2 つの認証情報を並べた設定。
    fn two_credentials(first: &str, second: &str) -> String {
        format!(
            r#"
[credentials.a]
type = "relay"
url = "{first}"
models = ["m"]

[credentials.b]
type = "relay"
url = "{second}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a", "b"]
"#
        )
    }

    /// 上限に当たったら次の認証情報を試す。
    ///
    /// 上限はアカウント単位なので、別の認証情報なら通る。
    #[tokio::test]
    async fn rate_limit_falls_back_to_the_next_credential() {
        let limited = FakeUpstream::always(429).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&limited.url, &spare.url)).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
        assert!(body_text(resp.response).await.contains("ok"));
        assert_eq!((limited.hits(), spare.hits()), (1, 1));
    }

    /// 認証が通らない先も次へ回す。
    ///
    /// upstream との認証の話なので、別の認証情報を持つ経路なら通る
    /// (クライアント側の認証は namespace のトークンで別に見ている)。
    #[tokio::test]
    async fn unauthorized_credential_falls_back_to_the_next() {
        let stale = FakeUpstream::always(401).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&stale.url, &spare.url)).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
        assert_eq!((stale.hits(), spare.hits()), (1, 1));
    }

    /// どの認証情報でも断られたら、最後に見た応答をそのまま返す。
    ///
    /// こちらで別の状態に置き換えると、クライアントが次の一手を決める
    /// 手掛かり (`retry-after` など) を失う。
    #[tokio::test]
    async fn the_last_denial_is_returned_when_every_credential_is_denied() {
        let first = FakeUpstream::always(429).await;
        let last = FakeUpstream::start(|_, _| {
            (
                429,
                r#"{"type":"error","error":{"message":"the last one"}}"#.to_owned(),
            )
        })
        .await;
        let gw = gateway(&two_credentials(&first.url, &last.url)).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429);
        assert_eq!((first.hits(), last.hits()), (1, 1), "どちらも試す");
        assert!(
            body_text(resp.response).await.contains("the last one"),
            "最後に断られた応答の本文がそのまま返る"
        );
    }

    /// 断られた応答のヘッダは落とさない。
    ///
    /// `retry-after` を消すと、クライアントはいつ再開してよいか分からなくなる。
    /// 状態コードを保ったまま返す意味の中心はここにある (DR-0009)。
    #[tokio::test]
    async fn a_passed_through_denial_keeps_its_retry_after() {
        let first = FakeUpstream::always(429).await;
        let last =
            FakeUpstream::start_with_headers(&[("retry-after", "30")], |_, _| (429, body_for(429)))
                .await;
        let gw = gateway(&two_credentials(&first.url, &last.url)).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429);
        assert_eq!(
            resp.response
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
                .map(|(_, v)| v.as_str()),
            Some("30"),
            "いつ再開できるかを伝える: {:?}",
            resp.response.headers
        );
    }

    /// 混んでいる (529) 先も次へ回す。
    ///
    /// 混み具合は宛先ごとに付く。ここに並ぶ経路は宛先が分かれている
    /// (Bedrock / Anthropic / 中継) ので、片方が詰まっていても、もう片方は
    /// 空いている (実測 2026-07-29)。
    #[tokio::test]
    async fn an_overloaded_upstream_falls_back_to_the_next() {
        let crowded = FakeUpstream::always(529).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&crowded.url, &spare.url)).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
        assert!(body_text(resp.response).await.contains("ok"));
        assert_eq!((crowded.hits(), spare.hits()), (1, 1));
    }

    /// どの宛先も混んでいたら、最後に見た 529 をそのまま返す。
    ///
    /// 503 に化けさせると「落ちている」という別の事実になり、
    /// クライアントのリトライの判断材料が変わる。
    #[tokio::test]
    async fn every_upstream_overloaded_returns_the_last_529() {
        let first = FakeUpstream::always(529).await;
        let last = FakeUpstream::start(|_, _| {
            (
                529,
                r#"{"type":"error","error":{"message":"the last one"}}"#.to_owned(),
            )
        })
        .await;
        let gw = gateway(&two_credentials(&first.url, &last.url)).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 529);
        assert_eq!((first.hits(), last.hits()), (1, 1), "どちらも試す");
        assert!(
            body_text(resp.response).await.contains("the last one"),
            "最後に見た応答の本文がそのまま返る"
        );
    }

    /// 経路が 1 本しかなければ、断られた応答をそのまま返す。
    #[tokio::test]
    async fn a_lone_denial_is_returned_as_is() {
        let limited = FakeUpstream::always(429).await;
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
            limited.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429);
        assert!(body_text(resp.response).await.contains("status 429"));
        assert_eq!(limited.hits(), 1, "同じ先へ送り直さない");
    }

    /// 経路断の次で断られたら、断られた応答を返す。
    #[tokio::test]
    async fn an_outage_followed_by_a_denial_returns_the_denial() {
        let down = FakeUpstream::always(503).await;
        let limited = FakeUpstream::always(429).await;
        let gw = gateway(&two_credentials(&down.url, &limited.url)).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429);
        assert_eq!((down.hits(), limited.hits()), (1, 1));
    }

    /// 断られた後に経路断が来ても、断られた応答は残る。
    ///
    /// 500 番台は応答を持ち回らないので、上書きされない。
    #[tokio::test]
    async fn a_denial_survives_a_later_outage() {
        let limited = FakeUpstream::always(429).await;
        let down = FakeUpstream::always(503).await;
        let gw = gateway(&two_credentials(&limited.url, &down.url)).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429, "経路断で塗り替えない");
        assert_eq!((limited.hits(), down.hits()), (1, 1));
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
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
        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!((flaky.hits(), alive.hits()), (1, 1));

        // 2 回目: flaky は復帰しているが、通った alive を先に試す。
        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!(
            (flaky.hits(), alive.hits()),
            (1, 2),
            "復帰した先より、通った先を優先する"
        );
    }

    /// 断られた後に、応答すら返らない失敗が来ても断られた応答は残る。
    ///
    /// 繋がらない先は応答を持たないので、保持しているものを上書きしない。
    #[tokio::test]
    async fn a_denial_survives_an_unreachable_route() {
        let limited = FakeUpstream::always(429).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{}"
models = ["m"]

[credentials.b]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a", "b"]
"#,
            limited.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429, "繋がらない先で塗り替えない");
        assert!(body_text(resp.response).await.contains("status 429"));
        assert_eq!(limited.hits(), 1);
    }

    /// beta を落として送り直した先で断られたら、そこも次へ回す。
    ///
    /// DR-0003 の送り直しと本 DR の切り替えが重なる地点。
    #[tokio::test]
    async fn a_denial_after_the_beta_retry_falls_back_too() {
        let negotiating = FakeUpstream::start(|n, _| match n {
            1 => (
                400,
                r#"{"type":"error","error":{"message":"invalid beta flag"}}"#.to_owned(),
            ),
            _ => (429, body_for(429)),
        })
        .await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway_with(
            &format!(
                r#"
[credentials.a]
type = "claude_oauth"
url = "{}"

[credentials.b]
type = "relay"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a", "b"]
"#,
                negotiating.url, spare.url
            ),
            StaticStore::new(),
        )
        .await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200, "断られた先を飛ばして次が通る");
        assert_eq!(negotiating.hits(), 2, "送り直すのは 1 回だけ");
        assert_eq!(spare.hits(), 1);
    }

    /// 送り直した先で断られ、他に経路が無ければ、その応答を返す。
    #[tokio::test]
    async fn a_denial_from_the_beta_retry_is_passed_through() {
        let up = FakeUpstream::start(|n, _| match n {
            1 => (
                400,
                r#"{"type":"error","error":{"message":"invalid beta flag"}}"#.to_owned(),
            ),
            _ => (
                429,
                r#"{"type":"error","error":{"message":"after the retry"}}"#.to_owned(),
            ),
        })
        .await;
        let gw = gateway_with(&oauth_config(&up.url), StaticStore::new()).await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429);
        assert_eq!(up.hits(), 2);
        assert!(
            body_text(resp.response).await.contains("after the retry"),
            "送り直した側の応答が返る"
        );
    }

    /// 断られた経路には貼り付かない。
    ///
    /// 覚えてしまうと、次の転送も上限に当たった先から始めることになる。
    #[tokio::test]
    async fn a_denied_route_is_not_remembered() {
        // 1 本目だけ断り、2 本目からは通る。覚えていれば次に先頭へ来る。
        let limited = FakeUpstream::then(429, 200).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&limited.url, &spare.url)).await;

        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!((limited.hits(), spare.hits()), (1, 1), "断られて次へ");

        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!(
            (limited.hits(), spare.hits()),
            (1, 2),
            "覚えるのは通った先だけ"
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
                NS,
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
                NS,
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
                NS,
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200, "落として送り直した結果が返る");
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
        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
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

        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();

        assert_eq!(resp.response.status, 400);
        assert!(
            body_text(resp.response).await.contains("status 400"),
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
            .forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
            .await
            .unwrap();
        assert_eq!(resp.response.status, 200);

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

        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), beta_header())
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

    /// 種別ごとに、利用状況をどこまで出せるか。
    #[test]
    fn support_depends_on_the_credential_type() {
        use crate::usage::Support;

        let oauth = CredentialSpec::ClaudeOauth {
            url: "https://api.anthropic.com".to_owned(),
            headers: Default::default(),
            exclude: Vec::new(),
        };
        let bedrock = CredentialSpec::ClaudeBedrock {
            url: "https://bedrock.invalid/anthropic".to_owned(),
            headers: Default::default(),
            deny_beta: None,
            exclude: Vec::new(),
        };
        let relay = CredentialSpec::Relay {
            url: "http://127.0.0.1:8317".to_owned(),
            headers: Default::default(),
            models: Vec::new(),
            exclude: Vec::new(),
        };

        assert_eq!(support_of(&oauth, false), Support::Unobserved);
        assert_eq!(support_of(&bedrock, false), Support::NotApplicable);
        assert_eq!(support_of(&relay, false), Support::UpstreamDependent);

        // 観測できているなら、種別に関わらずその値を出す。
        for spec in [&oauth, &bedrock, &relay] {
            assert_eq!(support_of(spec, true), Support::Observed);
        }
    }

    /// 使っていない credential も名前は出す。
    ///
    /// 消してしまうと「設定にあるが未観測」と「設定に無い」の区別がつかない。
    #[tokio::test]
    async fn usage_report_lists_every_credential() {
        let gw = gateway(
            r#"
[credentials.claude-personal]
type = "claude_oauth"

[credentials.bedrock]
type = "claude_bedrock"
url = "https://bedrock.invalid/anthropic"

[credentials.cpa]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m"]

[ns.default]
"#,
        )
        .await;

        let report = gw.usage_report(false).await;
        let names: Vec<&str> = report.credentials.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["bedrock", "claude-personal", "cpa"]);

        assert!(report.probe.is_none(), "既定では投げない");
        assert!(
            report.credentials.iter().all(|c| c.snapshot.is_none()),
            "転送していないので未観測"
        );
        assert!(
            report
                .credentials
                .iter()
                .all(|c| c.note.as_ref().is_some_and(|n| !n.is_empty())),
            "出せない理由が書いてある"
        );
    }

    /// プローブが失敗した credential は理由を載せ、他は返す。
    #[tokio::test]
    async fn a_failing_probe_does_not_take_the_others_down() {
        let gw = gateway(
            r#"
[credentials.nowhere]
type = "claude_oauth"
url = "http://127.0.0.1:9"

[credentials.bedrock]
type = "claude_bedrock"
url = "https://bedrock.invalid/anthropic"

[ns.default]
"#,
        )
        .await;

        let report = gw.usage_report(true).await;
        let probe = report.probe.expect("投げた記録が残る");
        assert_eq!(probe.requests, 1, "ヘッダを返すのは claude_oauth だけ");
        assert_eq!(probe.model, PROBE_MODEL);

        let by_name = |name: &str| {
            report
                .credentials
                .iter()
                .find(|c| c.name == name)
                .expect("設定にある")
                .clone()
        };
        assert!(
            by_name("nowhere").probe_error.is_some(),
            "繋がらなかった理由を載せる"
        );
        assert!(
            by_name("bedrock").probe_error.is_none(),
            "投げていない相手に失敗は書かない"
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
                NS,
                "/v1/messages",
                None,
                json!({"model": "opus"}),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(resp.response.status, 200);
        assert_eq!(up.hits(), 1);
    }
}
