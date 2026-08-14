//! リクエスト 1 本を捌く。
//!
//! モデル名から経路を選び、上から試して、通ったものの応答をそのまま返す。
//! 切り替えられるのは**クライアントへ 1 バイトも書く前**まで。応答を流し
//! 始めた後で upstream が切れても、HTTP のやり直しはできない。
//!
//! 経路がどうなっているか (断られた印・枠・様子見) は経路自身が持つ
//! (DR-0014 §3)。ここに経路の名前を鍵にした表は無く、通った / 断られたを
//! その経路へ伝えるだけ。

use std::sync::Arc;

use futures_util::StreamExt as _;
use serde_json::Value;
use tracing::{info, warn};

use crate::config::{Config, Namespace};
use crate::credential::time::now_unix;
use crate::credential::{Credential, CredentialId, CredentialStore, Persistence};
use crate::denial::Probing;
use crate::egress::{self, EgressRequest, Headers, Response, SentResponse};
use crate::error::UpstreamAttempt;
use crate::events::{self, Events};
use crate::exchange;
use crate::metering::{Pricing, PricingSource, TokenKind, TokenUsage, UsageObserver};
use crate::provider::{Admission, Preset, ProbeRequest};
use crate::quota::{self, QuotaLimit, QuotaStore};
use crate::router::{Route, Router, Selection};
use crate::session;
use crate::stats::{self, Stats};
use crate::tap::Tap;
use crate::{Error, Result};

pub struct Gateway<P: Persistence> {
    config: Config,
    router: Router,
    credentials: CredentialStore<P>,
    http: reqwest::Client,
    refresh_interval: std::time::Duration,
    /// 枠の観測を再起動を跨いで持つ置き場。裏で走る仕事とも共有する。
    usage: Arc<QuotaStore>,
    stats: Arc<Stats>,
    /// 転送のたびに起きたことを見ている人へ流す口 (DR-0012)。
    ///
    /// router も同じ口を持つ (自前で返す 429 を流すため)。
    events: Arc<Events>,
    tap: Arc<Tap>,
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
            // upstream とは ALPN で h2 になる。
            // TCP の keepalive は TLS の下を通るので、中身のバイト数
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
            .map_err(|e| Error::Config(format!("could not build the HTTP client: {e}")))?;

        // 知らせの口は 1 本。発火は各経路と router、束ねるのはここ (DR-0014 §3)。
        let events = Arc::new(Events::new());
        let tap = Arc::new(Tap::new());

        Ok(Self {
            refresh_interval: std::time::Duration::from_secs(config.discovery.refresh_secs),
            router: Router::new(config.clone(), Arc::clone(&events)),
            // 書き手の名前に待ち受け先を使う。同じ置き場を別ポートの gateway と
            // 共有しても、互いのファイルを書かない (DR-0011)。
            stats: Arc::new(Stats::new(
                config.stats.resolve_dir(),
                &config.server.listen,
            )),
            config: config.clone(),
            credentials: CredentialStore::new(persistence, http.clone()),
            http,
            // 利用状況も同じ置き場・同じ書き手の名前で持つ (DR-0007)。
            usage: Arc::new(QuotaStore::new(
                config.stats.resolve_dir(),
                &config.server.listen,
            )),
            events,
            tap,
        })
    }

    /// 使用量の日次集計。
    ///
    /// 受け取り口が中継に tap を挟むのに使う。閲覧の報告は
    /// [`Self::stats_report`] から取る。
    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    /// 日次集計の報告を作る (DR-0011)。
    ///
    /// USD は読み出しのたびに換算する。単価を知っているのは答えた経路の
    /// provider なので、集計の器には引き当て役を渡す (DR-0014 §4)。
    pub fn stats_report(&self, days: usize, now: i64) -> stats::Report {
        self.stats.report(days, now, &RoutePricing(&self.router))
    }

    /// 転送のたびに起きたことを流す口 (DR-0012)。
    ///
    /// 配信の口 (`/llm-gateway/events`) がここを見る。
    pub fn events(&self) -> &Arc<Events> {
        &self.events
    }

    /// 購読中だけ転送の詳細を流すデバッグ用 tap。
    pub fn tap(&self) -> &Arc<Tap> {
        &self.tap
    }

    /// 前回落とした分を読み戻す。待ち受けを始める前に 1 回だけ呼ぶ。
    ///
    /// 日次集計を読まずに数え直すと、次の保存で前回までの分を上書きして消す
    /// (DR-0011)。枠の観測を読まないと、再起動のたびに全 credential が未観測へ
    /// 戻る (DR-0007)。`now` を受けるのは、読み戻す「当日」を試験から固定するため。
    pub async fn restore(&self, now: i64) {
        self.stats.restore(now);
        self.usage.restore().await;

        // 読み戻した観測は、それを持つべき経路へ渡す。置き場はディスクとの
        // 出入りを担い、今の値は経路が持つ (DR-0014 §3)。
        for (name, preset) in self.router.presets() {
            if let Some(snapshot) = self.usage.get(&CredentialId::new(name)).await {
                preset.restore_quota(snapshot);
            }
        }
    }

    /// 変わった分をディスクへ落とす。
    ///
    /// 落とし損なっても止めない — 次の周回で書き直される。
    pub async fn save(&self) {
        if let Err(e) = self.stats.flush() {
            tracing::warn!(%e, "cannot save daily totals");
        }
        if let Err(e) = self.usage.save().await {
            tracing::warn!(%e, "cannot save usage");
        }
    }

    /// 一定の間隔で落とし続ける。
    ///
    /// 間隔を空けるのは、1 リクエストごとに書くのが無駄だから (kawaz 裁定)。
    /// 日次集計と利用状況を同じ周回で落とすので、書き込みが重なる先は
    /// この 1 つだけになる。
    pub async fn keep_saving(&self, every: std::time::Duration) {
        let mut ticker = tokio::time::interval(every);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            self.save().await;
        }
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
        let requested = egress::model_of(&body)?.to_owned();

        // `opus` のような短い名前は、ここで実際のモデル名に直す。
        // upstream はこの名前を知らないので、ボディも書き換える。
        let model = self.router.resolve(ns, &requested).await;
        if model != requested {
            egress::rewrite_model(&mut body, &model);
        }

        let session = session::derive(&body, &headers);
        // 知らせに載せる素性。会話の id はクライアントが名乗ったものを使う
        // (こちらが本文から作る affinity の鍵とは別物、DR-0012)。
        let call = Call {
            ns: ns_name,
            model: &model,
            session_id: session::declared_id(&headers),
            prefix: events::prefix(&body),
            path,
            query,
            body: &body,
        };
        let routes = self
            .router
            .routes_for(ns, ns_name, &model, &session)
            .await?;
        let now = now_unix();

        // 締め出している経路には、裏で様子を聞きに行く役を立てる。上限は
        // リセット時刻より早く開くこともあり、実リクエストを当てない限り
        // 開いたことに気づけない。聞き終わるのは待たない — この 1 本を
        // 遅くしてまで待つ価値は無く、結果は次のリクエストが使う。
        for route in &routes {
            if let Some(id) = &route.credential
                && let Some(probing) = route.preset.claim_probe(now)
            {
                self.probe_in_background(id, &route.preset, probing);
            }
        }

        // 断られている経路は飛ばす。全滅なら router が 429 を組んで返す
        // (「候補が空」は経路を選ぶ側の判断、DR-0014 §8)。
        let routes = match self.router.select(
            &routes,
            &model,
            now,
            &call.origin(crate::stats::NO_CREDENTIAL),
        ) {
            Selection::Ready(ready) => ready,
            Selection::AllDenied { response, .. } => {
                // 行き場を失った今が、状態を確かめる価値の最も高い瞬間。
                // 次の周期を待たずに聞きに行く。
                for route in &routes {
                    if let Some(id) = &route.credential
                        && let Some(probing) = route.preset.claim_ask(now)
                    {
                        self.probe_in_background(id, &route.preset, probing);
                    }
                }
                return Ok(Forwarded {
                    response,
                    credential: None,
                    route: "router".to_owned(),
                    model,
                    // 自前で組んだ断りなので、消費したトークンは無い。
                    usage: None,
                });
            }
        };

        let mut attempts = Vec::new();
        // この経路に断られた応答のうち、最後のもの。全滅したときに返す。
        // 認証情報を添えて持ち回るのは、どの経路を通って返る応答でも同じ形
        // (応答 + 身元) にするため。使用量の集計に乗るのは 2xx だけで、断られた
        // 応答は集計されない (エラーの本文に usage は無い、DR-0011)。
        let mut denied: Option<(
            Response,
            Option<CredentialId>,
            String,
            Option<crate::provider::ClientError>,
        )> = None;
        for route in &routes {
            match self.try_route(route, &call, &headers).await {
                Ok(resp) => {
                    // 貼り付けるのは通った経路だけ。断られた先を覚えると、
                    // 次の転送も同じところから始めることになる。
                    if resp.status / 100 == 2 {
                        self.router.remember(ns_name, &session, &model, route).await;
                        // 通ったなら締め出しの根拠は消えている。
                        route.preset.allow(&model);
                    }
                    // ここまでで届いているのはヘッダだけ。本文がクライアント
                    // まで流れ切ったかどうかは crate::exchange が記録する。
                    exchange::record_upstream_headers(
                        &tracing::Span::current(),
                        &model,
                        route.name(),
                        resp.status,
                    );
                    // 本文 usage の読み方は、答えた provider だけが知っている
                    // (DR-0014 §4)。受け取り口は preset を知らないので、読む役を
                    // 応答と一緒に持たせて渡す。
                    let usage = usage_observer(route.preset.as_ref(), &resp);
                    return Ok(Forwarded {
                        response: resp,
                        credential: route.credential.clone(),
                        route: route.name().to_owned(),
                        model,
                        usage,
                    });
                }
                Err(Switch {
                    reason,
                    denial,
                    client_error,
                }) => {
                    warn!(model = %model, route = route.name(), %reason, "switching routes");
                    attempts.push(UpstreamAttempt {
                        provider: route.name().to_owned(),
                        reason,
                    });
                    if let Some(resp) = denial {
                        let (resp, body) = match egress::buffer(resp).await {
                            Ok(buffered) => buffered,
                            Err(error) => {
                                warn!(route = route.name(), %error, "cannot read the refused response body");
                                continue;
                            }
                        };
                        // 時間が経てば空く断りなら、次のリクエストで同じ壁に
                        // 当たらないよう期限を控える。読み方は provider が持つ。
                        let now = now_unix();
                        if let Some(denial) = route.preset.reject(
                            resp.status,
                            &resp.headers,
                            Some(&body),
                            &model,
                            now,
                        ) {
                            warn!(
                                route = route.name(),
                                status = resp.status,
                                reason = ?denial.reason,
                                seconds = denial.until - now,
                                "この経路を候補から外します"
                            );

                            // 断られたのに理由が応答に無いなら、枠を聞きに行く。
                            // 当て推量の 60 秒を、どの枠がいつ開くかという
                            // 実際の答えに差し替えられる (DR-0007)。
                            if denial.reason == crate::denial::Reason::Busy
                                && let Some(id) = &route.credential
                                && let Some(probing) = route.preset.claim_ask(now)
                            {
                                self.probe_in_background(id, &route.preset, probing);
                            }
                        }
                        denied = Some((
                            resp,
                            route.credential.clone(),
                            route.name().to_owned(),
                            client_error.map(|error| *error),
                        ));
                    }
                }
            }
        }

        // 断られた応答を見ていたなら、最後のものをそのまま返す。こちらで
        // 別の状態に置き換えると、`retry-after` のようなクライアントが次の
        // 一手を決める手掛かりまで消える。
        if let Some((resp, credential, route, client_error)) = denied {
            let resp = match client_error {
                Some(error) => client_error_response(resp, error)?,
                None => resp,
            };
            warn!(
                model = %model,
                status = resp.status,
                routes = routes.len(),
                "経路を使い切りました。最後に断られた応答をそのまま返します"
            );
            return Ok(Forwarded {
                response: resp,
                credential,
                route,
                model,
                // 断られた応答に usage は載らない (DR-0011)。
                usage: None,
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
        call: &Call<'_>,
        headers: &[(String, String)],
    ) -> std::result::Result<Response, Switch> {
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
        let learned: Vec<String> = credential
            .as_ref()
            .map(|c| c.denied_beta.iter().cloned().collect())
            .unwrap_or_default();
        let negotiation = route.preset.negotiation();

        let mut sending = Headers::new(headers.to_vec());
        let sent = negotiation
            .map(|n| n.prepare(&mut sending, &learned))
            .unwrap_or_default();

        let resp = self.send(route, call, credential.as_ref(), sending).await?;
        let resp = self.admit(route, call.model, resp).await?;

        // 交渉するものを載せていないなら、失敗の原因は他にある。
        if resp.status != 400 || sent.is_empty() {
            return accept_or_switch(resp);
        }
        let Some(negotiation) = negotiation else {
            return accept_or_switch(resp);
        };

        let (resp, raw) = egress::buffer(resp)
            .await
            .map_err(|e| Switch::to_next(e.to_string()))?;
        let raw = String::from_utf8_lossy(&raw);
        let Some(blamed) = negotiation.blame(&raw, &sent) else {
            return accept_or_switch(resp);
        };

        warn!(
            route = route.name(),
            flags = ?blamed,
            "送ったヘッダが拒否されました。落として送り直します"
        );
        if let Some(id) = &route.credential
            && let Err(e) = self.credentials.record_denied_beta(id, &blamed).await
        {
            // 覚えられなくても転送は続ける。次も同じ失敗を 1 回踏むだけで、
            // ここで諦めるとクライアントには何も返らない。
            warn!(credential = %id, %e, "cannot save the denied flag");
        }

        let mut retrying = Headers::new(headers.to_vec());
        let mut learned = learned;
        learned.extend(blamed);
        negotiation.prepare(&mut retrying, &learned);

        // 送り直すのは 1 回だけ。これでも失敗ならクライアントへ返す。
        let resp = self
            .send(route, call, credential.as_ref(), retrying)
            .await?;
        let resp = self.admit(route, call.model, resp).await?;
        accept_or_switch(resp)
    }

    async fn admit(
        &self,
        route: &Arc<Route>,
        model: &str,
        sent: SentResponse,
    ) -> std::result::Result<Response, Switch> {
        let mode = sent.mode;
        match route
            .preset
            .admit(sent.response, model, now_unix())
            .await
            .map_err(|e| Switch::to_next(e.to_string()))?
        {
            Admission::Admitted(response) => {
                egress::finish_response(SentResponse { response, mode })
                    .await
                    .map_err(|e| Switch::to_next(e.to_string()))
            }
            Admission::Rejected {
                response,
                reason,
                client_error,
                ..
            } => {
                let response = egress::finish_response(SentResponse { response, mode })
                    .await
                    .map_err(|e| Switch::to_next(e.to_string()))?;
                Err(Switch {
                    reason,
                    denial: Some(response),
                    client_error: client_error.map(Box::new),
                })
            }
        }
    }

    async fn send(
        &self,
        route: &Arc<Route>,
        call: &Call<'_>,
        credential: Option<&Credential>,
        headers: Headers,
    ) -> std::result::Result<SentResponse, Switch> {
        // upstream での名前がクライアントの名前と違う経路にだけ、書き換えて
        // 送る。何という名前で受け付けるかは discovery が答えている。
        let mut body = call.body.clone();
        if let Some(upstream) = &route.upstream_model {
            egress::rewrite_model(&mut body, upstream);
        }

        let resp = egress::send(
            &self.http,
            route.preset.as_ref(),
            credential,
            EgressRequest {
                path: call.path.to_owned(),
                query: call.query.map(str::to_owned),
                body,
                headers,
            },
        )
        .await
        .map_err(|e| Switch::to_next(e.to_string()))?;

        // 便乗して枠を拾う (DR-0007)。読むのはヘッダだけなので、本文はこの後も
        // そのまま流れる。上限に当たった応答こそ見たいので、status では絞らない。
        let now = now_unix();
        if let Some(id) = &route.credential
            && let Some(snapshot) = route.preset.observe_quota(&resp.response.headers, now)
        {
            self.usage.observe(id, snapshot).await;
        }

        // 見ている人へ知らせる (DR-0012)。upstream がヘッダを返したこの瞬間が、
        // prompt cache の 5 分が走り始めた時刻に一番近い。断られた応答も流す
        // ので、status で絞らない。
        self.events.publish(events::Event::new(
            now,
            &call.origin(route.name()),
            resp.response.status,
        ));
        Ok(resp)
    }

    /// credential ごとの利用状況。
    ///
    /// `probe` が真なら、先に能動プローブを投げてから作る。既定を便乗のみに
    /// するのは、usage の確認が usage を勝手に消費する構図を避けるため
    /// (DR-0007)。
    pub async fn usage_report(&self, probe: bool) -> quota::Report {
        let probed = if probe {
            self.probe_usage().await
        } else {
            None
        };

        let mut credentials = Vec::new();
        for (name, route) in &self.config.routes {
            // 今の観測も、観測が無いときに何と言えるかも、持っているのは経路
            // (DR-0014 §3)。置き場はディスクとの出入りだけを担う。
            let preset = self.router.preset(name);
            let snapshot = preset.and_then(|preset| preset.quota());
            let support = preset.map_or(
                // 経路を組めなかった名前について、こちらから言えることは無い。
                quota::Support::UpstreamDependent,
                |preset| preset.quota_support(),
            );
            let credential_type = route
                .credential(&self.config)
                .map_or("none", crate::config::CredentialSpec::type_name);

            let mut entry = quota::CredentialUsage::new(name, credential_type, support, snapshot);
            entry.limits = probed
                .as_ref()
                .and_then(|p| p.limits.get(name.as_str()).cloned());
            entry.probe_error = probed
                .as_ref()
                .and_then(|p| p.errors.get(name.as_str()).cloned());
            credentials.push(entry);
        }

        let mut report = quota::Report::new(now_unix(), credentials);
        report.probe = probed.map(|p| p.spent);
        report
    }

    /// 枠を聞ける経路に、最小のリクエストを 1 本ずつ投げる。
    ///
    /// 失敗した credential はその理由を控えて先へ進む。1 つの認証切れで
    /// 一覧全体が返らなくなると、確認したかった他の credential まで見えない。
    async fn probe_usage(&self) -> Option<Probed> {
        let mut spent = quota::Probe::default();
        let mut errors = std::collections::BTreeMap::new();
        let mut limits = std::collections::BTreeMap::new();

        for (name, preset) in self.router.presets() {
            if preset.quota_api().is_none() {
                continue;
            }
            let Some(credential_name) = self
                .config
                .routes
                .get(name)
                .and_then(|route| route.credential.as_deref())
            else {
                continue;
            };
            let id = CredentialId::new(credential_name);
            // 枠を聞くのが先。こちらはトークンを使わないので、この後の
            // 最小リクエストが失敗しても、枠だけは見えるようにしておく。
            if let Some(found) = self.ask_limits(&id, preset).await {
                limits.insert(name.to_owned(), found);
            }

            let Some(probe) = preset.probe_request() else {
                continue;
            };
            spent.requests += 1;
            spent.model = probe.model.clone();
            match self.probe_one(&id, preset, probe).await {
                Ok((input, output)) => {
                    spent.input_tokens += input;
                    spent.output_tokens += output;
                }
                Err(reason) => {
                    warn!(route = %name, credential = %id, %reason, "cannot fetch usage");
                    errors.insert(name.to_owned(), reason);
                }
            }
        }
        Some(Probed {
            spent,
            errors,
            limits,
        })
    }

    /// 1 つの経路の枠を、専用の口に聞く。
    ///
    /// トークンを使わないので、聞くこと自体が枠を減らさない。読めなければ
    /// `None` を返して先へ進む — 枠が見えないのは不便だが、それで一覧全体を
    /// 返せなくする理由にはならない。
    async fn ask_limits(&self, id: &CredentialId, preset: &Preset) -> Option<Vec<QuotaLimit>> {
        let api = preset.quota_api()?;
        let credential = match self.credentials.acquire(id).await {
            Ok(credential) => credential,
            Err(e) => {
                warn!(credential = %id, %e, "cannot prepare credentials to query the quota");
                return None;
            }
        };
        api.fetch(&self.http, &credential).await.ok()
    }

    /// 1 つの経路に投げて、ヘッダを拾う。返すのは消費したトークン。
    async fn probe_one(
        &self,
        id: &CredentialId,
        preset: &Preset,
        probe: ProbeRequest,
    ) -> std::result::Result<(u64, u64), String> {
        let sample = self.sound(id, preset, probe).await?;
        if sample.status != 200 {
            return Err(format!(
                "upstream returned {}: {}",
                sample.status, sample.body
            ));
        }
        Ok((
            sample.usage.get(&TokenKind::input()).unwrap_or(0),
            sample.usage.get(&TokenKind::output()).unwrap_or(0),
        ))
    }

    /// 経路の枠を、裏で聞きに行く。
    ///
    /// 実リクエストは断られている経路に当てない。代わりに枠照会 API
    /// ([`crate::provider::QuotaApi`]) に聞き、返ってきた枠で印を引き直す。
    /// **トークンを使わない**ので、聞くこと自体が枠を減らさない。
    ///
    /// 要求から切り離した仕事として走らせる。要求は途中で消える (クライアントが
    /// 切る) が、聞きに行った結果は次のリクエストのために残したい
    /// ([`crate::credential`] の更新と同じ形)。
    ///
    /// 返すのは走らせた仕事。転送の側は待たない (待つと、この 1 本が聞き終わる
    /// まで遅くなる) が、試験は終わりを見届けられる。
    ///
    /// 札を取れるのは枠照会 API を持つ経路だけなので、ここへ来る相手には
    /// 必ず聞く口がある。
    fn probe_in_background(
        &self,
        id: &CredentialId,
        preset: &Arc<Preset>,
        probing: Probing,
    ) -> tokio::task::JoinHandle<()> {
        let id = id.clone();
        let preset = Arc::clone(preset);
        let http = self.http.clone();
        let credentials = self.credentials.clone();

        tokio::spawn(async move {
            // 札は、走り切っても落ちても [`Drop`] で外れる。
            let _probing = probing;
            let credential = match credentials.acquire(&id).await {
                Ok(credential) => credential,
                Err(e) => {
                    warn!(credential = %id, %e, "cannot prepare credentials to query the quota");
                    return;
                }
            };
            let Some(api) = preset.quota_api() else {
                return;
            };
            let Ok(limits) = api.fetch(&http, &credential).await else {
                // 読めなかった。印は据え置き、期限が来たときの実リクエストに
                // 判断を任せる。
                return;
            };

            preset.apply_quota(&limits, now_unix());
            info!(
                credential = %id,
                limits = limits.len(),
                "枠を聞いて締め出しを引き直しました"
            );
        })
    }

    /// 最小のリクエストを 1 本投げて、返ってきたものを持ち帰る。
    ///
    /// 何を投げるかは経路が組んだもの ([`ProbeRequest`]) をそのまま使う。
    /// ここで本文やヘッダを作ると、方言を 1 つ知っていることになる。
    async fn sound(
        &self,
        id: &CredentialId,
        preset: &Preset,
        probe: ProbeRequest,
    ) -> std::result::Result<Sample, String> {
        let credential = self
            .credentials
            .acquire(id)
            .await
            .map_err(|e| e.to_string())?;

        let sent = egress::send(&self.http, preset, Some(&credential), probe.request)
            .await
            .map_err(|e| e.to_string())?;
        let resp = egress::finish_response(sent)
            .await
            .map_err(|e| e.to_string())?;

        // 上限に当たった応答にも使用率は載る。状態を見る前に拾っておく。
        let status = resp.status;
        let content_type = resp.headers.get("content-type").map(str::to_owned);
        if let Some(snapshot) = preset.observe_quota(&resp.headers, now_unix()) {
            self.usage.observe(id, snapshot).await;
        }

        let raw = egress::collect_body(resp.body)
            .await
            .map_err(|e| e.to_string())?;

        // 消費したトークンの読み方も provider が持つ (DR-0014 §4)。ここで
        // フィールド名を知っていると、方言ごとに分岐が増える。
        let usage = preset
            .metering()
            .usage_observer(content_type.as_deref())
            .and_then(|mut observer| {
                observer.observe(&raw);
                observer.finish()
            })
            .unwrap_or_default();

        Ok(Sample {
            status,
            usage,
            body: String::from_utf8_lossy(&raw)
                .chars()
                .take(200)
                .collect::<String>(),
        })
    }
}

/// 受けた 1 本の呼び出し。経路を試すたびに要るものをまとめて持ち回る。
///
/// 個別の引数で渡していくと、経路を試す関数の引数が増え続ける (どの経路でも
/// 同じ中身を渡すので、増えるのは呼び出し側の写経だけになる)。
struct Call<'a> {
    ns: &'a str,
    /// 解決後の実モデル名。
    model: &'a str,
    /// クライアントが名乗った会話の id。名乗らない相手もいる。
    session_id: Option<String>,
    /// 会話系列の識別子。経路を試すたびに作り直さないよう、1 回で取る。
    prefix: Option<String>,
    path: &'a str,
    query: Option<&'a str>,
    body: &'a Value,
}

impl<'a> Call<'a> {
    /// 知らせに載せる素性。答えた経路の名前だけが呼び出しごとに変わる。
    fn origin(&'a self, credential: &'a str) -> events::Origin<'a> {
        events::Origin {
            session_id: self.session_id.as_deref(),
            prefix: self.prefix.as_deref(),
            ns: self.ns,
            model: self.model,
            credential,
        }
    }
}

/// プローブが持ち帰ったもの。
struct Sample {
    status: u16,
    /// 消費したトークン。2xx のときだけ載る。
    usage: TokenUsage,
    /// 本文の頭。断られた理由を説明に使う。
    body: String,
}

/// 転送した結果。応答と、それを出した経路の身元。
///
/// 身元を応答と別に持つのは、使用量の集計が「どの credential のどのモデルか」で
/// 束ねるのに対し、[`Response`] は HTTP の応答そのもの (状態・ヘッダ・本文) を
/// 表すため。集計の都合を応答の型に混ぜると、転送に関係のない項目が
/// upstream の応答を表す構造体に溜まっていく (DR-0011)。
pub struct Forwarded {
    pub response: Response,
    /// この応答を出した credential。relay 型のように認証情報を持たない経路は
    /// `None`。
    pub credential: Option<CredentialId>,
    /// この応答を出した経路名。
    pub route: String,
    /// 解決後の実モデル名。短い名前 (`opus`) はここでは解決済み。
    pub model: String,
    /// この応答の本文から usage を読む役。集計しない応答では `None`。
    ///
    /// 作れるのは応答を出した provider だけなので、応答と一緒に持ち回る。
    /// 本文へ挟むのは受け取り口 ([`crate::exchange::observe`])。
    pub usage: Option<Box<dyn UsageObserver>>,
}

impl std::fmt::Debug for Forwarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forwarded")
            .field("response", &self.response)
            .field("credential", &self.credential)
            .field("route", &self.route)
            .field("model", &self.model)
            .field("usage", &self.usage.is_some())
            .finish()
    }
}

/// この応答の本文から usage を読む役を作る。
///
/// 読めない content-type かどうかは provider が決める。ここが決めるのは
/// **2xx だけを覗く**ことだけ — エラーの本文に usage は載らないので、読んでも
/// 取れないものに層を重ねない (DR-0011)。
fn usage_observer(preset: &Preset, resp: &Response) -> Option<Box<dyn UsageObserver>> {
    if resp.status / 100 != 2 {
        return None;
    }
    preset
        .metering()
        .usage_observer(resp.headers.get("content-type"))
}

/// プローブの結果。消費した分と、credential ごとの失敗。
struct Probed {
    spent: quota::Probe,
    errors: std::collections::BTreeMap<String, String>,
    /// 専用の口から聞いた枠。聞けた credential の分だけ入る。
    limits: std::collections::BTreeMap<String, Vec<QuotaLimit>>,
}

/// 集計の 1 行の単価を、その行を出した経路へ聞く役。
///
/// 単価表は provider の側にあり (DR-0014 §4)、経路を引けるのは router なので、
/// 両者を繋ぐのがここの仕事になる。
struct RoutePricing<'a>(&'a Router);

impl PricingSource for RoutePricing<'_> {
    fn pricing(&self, credential: &str, model: &str) -> Option<Pricing> {
        if let Some(preset) = self.0.preset(credential) {
            return preset.metering().pricing(model);
        }
        // 認証情報を持たない経路 ([`crate::stats::NO_CREDENTIAL`]) や、設定から
        // 消えた名前の分。どの経路が答えたかは記録に残らないので、そのモデルに
        // 値を付けられる経路を探す。付けられる経路が 1 つも無ければ欄は出ない。
        self.0
            .presets()
            .find_map(|(_, preset)| preset.metering().pricing(model))
    }
}

fn client_error_response(
    mut response: Response,
    error: crate::provider::ClientError,
) -> Result<Response> {
    response.status = error.status;
    response.headers.set("content-type", "application/json");
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "error",
        "error": {"type": error.kind, "message": error.message}
    }))?;
    response.body = futures_util::stream::once(async move { Ok(bytes::Bytes::from(body)) }).boxed();
    Ok(response)
}

/// 次の経路へ回す理由。
struct Switch {
    reason: String,
    /// この経路に断られた応答。本文から provider 固有の期限を読んだ後も、
    /// 次の経路が全滅したときにクライアントへ返せる形で持ち回る。
    denial: Option<Response>,
    client_error: Option<Box<crate::provider::ClientError>>,
}

impl Switch {
    /// 応答を伴わない切り替え (経路断・送信できなかった等)。
    fn to_next(reason: String) -> Self {
        Self {
            reason,
            denial: None,
            client_error: None,
        }
    }
}

/// この応答をクライアントへ返すか、別の経路を試すか。
fn accept_or_switch(resp: Response) -> std::result::Result<Response, Switch> {
    let reason = format!("upstream returned {}", resp.status);
    if should_try_next(resp.status) {
        return Err(Switch::to_next(reason));
    }
    if is_route_denial(resp.status) {
        return Err(Switch {
            reason,
            denial: Some(resp),
            client_error: None,
        });
    }
    Ok(resp)
}

/// この状態なら別の upstream を試す価値があるか。
///
/// 経路が断たれている場合だけ切り替える。応答を持ち回る値打ちのない失敗で、
/// 中身は捨てる。断られた応答を残したまま切り替えるものは
/// [`is_route_denial`] が見る。
fn should_try_next(status: u16) -> bool {
    // 501 (未実装) は除く。別の経路に替えても実装されていないものは動かない。
    matches!(status, 500 | 502..=504)
}

/// この経路には断られたが、別の経路なら通りうるか。
///
/// 上限もトークンの有効性も混み具合も、この経路の向こう側 (アカウントと
/// 宛先) に付く。並んでいる認証情報は別のアカウントで、宛先も分かれているので、
/// ここが断ったことは次が断ることを意味しない。
///
/// - 401 / 403: upstream との認証の話。クライアント側の認証は gateway が
///   namespace のトークンで別に確かめている
/// - 429: 上限はアカウント単位
/// - 529: 宛先の混み具合。宛先が分かれている構成では、片方が詰まっていても
///   もう片方は空いている (実測 2026-07-29)
///
/// 応答は捨てずに持ち回る。全部断られたときは、これをそのまま返す。
fn is_route_denial(status: u16) -> bool {
    matches!(status, 401 | 403 | 429 | 529)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::StoredCredential;
    use crate::credential::stored::{CodexTokens, OauthTokens, Payload};
    use crate::credential::time::{format_rfc3339, now_unix};
    use crate::denial::{self, Availability, Denial, Reason, Scope};
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
        /// 届いた要求の数を配る合図。裏で走る仕事の到着を、時間で待たずに掴む。
        ///
        /// 数が積み上がる形にしておく。「1 本届いた」を配るだけだと、待ち始める
        /// 前に 2 本届いた場合に 2 本目を取り逃がす。
        arrived: Arc<tokio::sync::Semaphore>,
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
            Self::start_with_content_type("application/json", extra, respond).await
        }

        async fn start_sse(
            respond: impl Fn(usize, &str) -> (u16, String) + Send + Sync + 'static,
        ) -> Self {
            Self::start_with_content_type("text/event-stream", &[], respond).await
        }

        async fn start_with_content_type(
            content_type: &'static str,
            extra: &[(&str, &str)],
            respond: impl Fn(usize, &str) -> (u16, String) + Send + Sync + 'static,
        ) -> Self {
            let extra: Arc<String> =
                Arc::new(extra.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let arrived = Arc::new(tokio::sync::Semaphore::new(0));
            let counter = Arc::clone(&hits);
            let seen = Arc::clone(&requests);
            let bell = Arc::clone(&arrived);
            let respond = Arc::new(respond);

            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let counter = Arc::clone(&counter);
                    let seen = Arc::clone(&seen);
                    let bell = Arc::clone(&bell);
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
                            // 数を足すので、待ち始めるのが後でも取り逃がさない。
                            bell.add_permits(1);
                            answer
                        };

                        let resp = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\n\
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
                arrived,
            }
        }

        /// 次の要求が届くまで待つ。
        async fn next_request(&self) {
            self.arrived.acquire().await.expect("閉じない").forget();
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        /// 受け取った要求 (ヘッダを含む生のまま)。
        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }

        /// 転送だけを数える。裏で走る枠の問い合わせと混ぜない。
        fn forwards(&self) -> usize {
            self.requests()
                .iter()
                .filter(|req| req.starts_with("POST /v1/messages"))
                .count()
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

    fn valid_codex_credential() -> StoredCredential {
        StoredCredential::new(Payload::CodexOauth(CodexTokens {
            oauth: OauthTokens {
                access_token: "tok".into(),
                refresh_token: "rt".into(),
                expired: "2099-01-01T00:00:00Z".into(),
                email: "a@b.c".into(),
                extra: Default::default(),
            },
            id_token: None,
            account_id: Some("acc-1".to_owned()),
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

    /// 名前で経路の preset を引く。状態 (締め出し・枠) を持っている実体。
    fn preset_of<'a, P: Persistence>(gw: &'a Gateway<P>, name: &str) -> &'a Arc<Preset> {
        gw.router.preset(name).expect("設定にある")
    }

    /// 転送の試験で使う namespace 名。
    const NS: &str = crate::config::DEFAULT_NAMESPACE;

    /// 既定の namespace。
    fn ns<P: Persistence>(gw: &Gateway<P>) -> &Namespace {
        gw.namespace(NS).expect("既定は必ずある")
    }

    /// 試験のリクエストが名乗るモデル。
    const MODEL: &str = "m";

    /// 窓が塞がっている印 (経路全体)。
    fn window_closed(until: i64) -> Denial {
        Denial {
            until,
            reason: Reason::Limited,
            scope: Scope::Everything,
        }
    }

    fn request() -> Value {
        json!({
            "model": "m",
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": r#"{"session_id":"s1"}"#},
        })
    }

    async fn body_text(resp: Response) -> String {
        String::from_utf8(egress::collect_body(resp.body).await.unwrap()).unwrap()
    }

    /// 経路が断たれた状態だけ次へ回す。
    #[test]
    fn switches_only_on_upstream_outage() {
        for status in [500, 502, 503, 504] {
            assert!(should_try_next(status), "{status} は経路断とみなす");
        }
        assert!(!should_try_next(501), "未実装は別の経路でも未実装");
        for status in [200, 201, 204, 400, 404, 422] {
            assert!(!should_try_next(status), "{status} は次を試さない");
        }
    }

    /// この経路に断られた分は、経路断とは別に扱う (応答を持ち回る側)。
    #[test]
    fn route_denials_are_told_apart_from_outages() {
        for status in [401, 403, 429, 529] {
            assert!(is_route_denial(status), "{status} はこの経路が断った");
            assert!(!should_try_next(status), "{status} は経路断ではない");
        }
        for status in [200, 400, 404, 422, 500, 502, 503, 504] {
            assert!(!is_route_denial(status), "{status} は経路のせいではない");
        }
    }

    #[tokio::test]
    async fn forwards_to_the_first_route() {
        let up = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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

    /// 通った応答には、本文 usage を読む役が付いてくる。
    ///
    /// 役を作れるのは方言を知っている provider だけなので、受け取り口が
    /// 自前で読む形にはしない (DR-0014 §4)。
    #[tokio::test]
    async fn a_successful_response_carries_a_usage_observer() {
        let up = FakeUpstream::always(200).await;
        let gw = gateway(&one_credential(&up.url)).await;

        let forwarded = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(forwarded.response.status, 200);
        assert!(forwarded.usage.is_some(), "集計する役が付く");
    }

    /// 断られた応答には役を付けない。エラーの本文に usage は載らない。
    #[tokio::test]
    async fn a_denied_response_carries_no_usage_observer() {
        let up = FakeUpstream::always(429).await;
        let gw = gateway(&one_credential(&up.url)).await;

        let forwarded = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(forwarded.response.status, 429);
        assert!(forwarded.usage.is_none(), "読んでも取れないものを覗かない");
    }

    /// 自前で組んだ 429 (全滅) にも役は付かない。upstream を叩いていない。
    #[tokio::test]
    async fn the_self_made_denial_carries_no_usage_observer() {
        let up = FakeUpstream::always(200).await;
        let gw = gateway(&one_credential(&up.url)).await;

        let now = now_unix();
        preset_of(&gw, "a").deny(window_closed(now + 100), now);

        let forwarded = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(forwarded.response.status, 429);
        assert!(forwarded.usage.is_none());
        assert_eq!(up.hits(), 0, "叩いていない");
    }

    /// 通った経路の認証情報が応答に付いてくる。使用量をこの鍵で束ねる。
    #[tokio::test]
    async fn a_forwarded_response_names_the_credential_that_answered() {
        let up = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"
url = "{}"

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
[routes.a]
provider = "anthropic"
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
[routes.a]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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

    /// 断られた応答を透過するときも、同じ形 (応答 + 身元) で返る。
    ///
    /// 身元が付くのは形を揃えるためで、集計に乗るという意味ではない
    /// (断られた応答は集計されない、DR-0011)。
    #[tokio::test]
    async fn a_passed_through_denial_still_names_the_route() {
        let denying = FakeUpstream::always(429).await;
        let gw = gateway(&format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
        assert_eq!(resp.credential, None, "relay 型なので持ち主なし");
    }

    /// 本文内エラーだけを Anthropic error JSON へ変え、upstream の message は失わない。
    #[tokio::test]
    async fn an_in_body_error_becomes_an_anthropic_error_response() {
        let response = Response {
            status: 200,
            headers: Headers::new(vec![(
                "content-type".to_owned(),
                "text/event-stream".to_owned(),
            )]),
            body: futures_util::stream::empty().boxed(),
        };
        let response = client_error_response(
            response,
            crate::provider::ClientError {
                status: 502,
                kind: "api_error".to_owned(),
                message: "upstream detail".to_owned(),
            },
        )
        .unwrap();

        assert_eq!(response.status, 502);
        assert_eq!(
            response.headers.get("content-type"),
            Some("application/json")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body_text(response).await).unwrap(),
            serde_json::json!({
                "type": "error",
                "error": {"type": "api_error", "message": "upstream detail"}
            })
        );
    }

    /// 経路が断たれていたら次を試す。
    #[tokio::test]
    async fn falls_back_when_upstream_is_down() {
        let down = FakeUpstream::always(503).await;
        let alive = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[routes.down]
provider = "anthropic"
url = "{}"
models = ["m"]

[routes.alive]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["down", "alive"]
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
[routes.nowhere]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["m"]

[routes.alive]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["nowhere", "alive"]
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
[routes.a]
provider = "anthropic"
url = "{}"
models = ["m"]

[routes.b]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a", "b"]
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
    /// 1 経路だけの設定。経路の選択が絡まない試験に使う。
    fn one_credential(url: &str) -> String {
        format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{url}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
"#
        )
    }

    fn two_credentials(first: &str, second: &str) -> String {
        format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{first}"
models = ["m"]

[routes.b]
provider = "anthropic"
url = "{second}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a", "b"]
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
            resp.response.headers.get("retry-after"),
            Some("30"),
            "いつ再開できるかを伝える: {:?}",
            resp.response.headers
        );
    }

    /// 一度断られた経路は、次のリクエストでは飛ばす。
    ///
    /// 上限に当たった認証情報が先頭にいると、印が無い限り**毎回** そこで
    /// 1 往復を捨ててから次へ回ることになる (DR-0009)。
    #[tokio::test]
    async fn a_rate_limited_route_is_skipped_next_time() {
        let limited = FakeUpstream::always(429).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&limited.url, &spare.url)).await;

        for _ in 0..2 {
            let resp = gw
                .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
                .await
                .unwrap();
            assert_eq!(resp.response.status, 200);
        }

        assert_eq!(
            (limited.hits(), spare.hits()),
            (1, 2),
            "2 回目は上限に当たった先を叩かない"
        );
    }

    /// あるモデルで断られても、同じ認証情報の他のモデルは使い続ける。
    ///
    /// 実測 (2026-07-31): 同じアカウントで haiku は 200、fable / opus /
    /// sonnet は 429 という状態が起きる。断られたモデルの都合で認証情報ごと
    /// 締め出すと、使えるはずの経路を自分で閉じることになる。
    #[tokio::test]
    async fn a_denial_for_one_model_does_not_close_the_others() {
        // fable だけ断り、haiku には応じる upstream。
        let picky = FakeUpstream::start(|_, req| {
            if req.contains("m-fable") {
                (429, body_for(429))
            } else {
                (200, body_for(200))
            }
        })
        .await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{}"
models = ["m-fable", "m-haiku"]

[routes.b]
provider = "anthropic"
url = "{}"
models = ["m-fable", "m-haiku"]

[[ns.default.routing]]
models = ["*"]
routes = ["a", "b"]
"#,
            picky.url, spare.url
        ))
        .await;

        let ask = |model: &str| {
            let body = json!({
                "model": model,
                "max_tokens": 8,
                "messages": [{"role": "user", "content": "hi"}],
                "metadata": {"user_id": r#"{"session_id":"s1"}"#},
            });
            gw.forward(ns(&gw), NS, "/v1/messages", None, body, vec![])
        };

        for _ in 0..2 {
            assert_eq!(ask("m-fable").await.unwrap().response.status, 200);
        }
        assert_eq!(ask("m-haiku").await.unwrap().response.status, 200);

        assert_eq!(
            (picky.hits(), spare.hits()),
            (2, 2),
            "fable は 1 回断られたら飛ばすが、haiku は同じ認証情報で試す"
        );
        assert_eq!(
            preset_of(&gw, "a").availability("m-haiku", now_unix()),
            Availability::Ready,
            "断られていないモデルに印は付かない"
        );
    }

    /// 経路を切り替えたときは、当たった先ごとに 1 通ずつ流れる。
    ///
    /// 5 分を数える相手にとって、意味があるのは**最後に通った先**の時刻。
    /// 断られた分も混ぜて流し、どれを使うかは見る側が status で決める
    /// (DR-0012)。
    #[tokio::test]
    async fn every_upstream_answer_is_announced() {
        let limited = FakeUpstream::always(429).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&limited.url, &spare.url)).await;

        let mut watching = gw.events().subscribe();
        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        let first = watching.recv().await.unwrap();
        assert_eq!((first.credential.as_str(), first.status), ("a", 429));

        let second = watching.recv().await.unwrap();
        assert_eq!((second.credential.as_str(), second.status), ("b", 200));
        assert_eq!(second.ns, NS);
        assert_eq!(second.model, "m");
        assert_eq!(second.session_id, None, "ヘッダを付けていない");
    }

    /// 自前で返した 429 も、見ている人には 1 通流れる (DR-0014 §8)。
    ///
    /// upstream を叩いていないだけで、クライアントには断りが返っている。
    /// 流さないと、webhook や SSE から「返事が消えた」ように見える。
    #[tokio::test]
    async fn the_self_made_denial_is_announced_too() {
        let far = FakeUpstream::always(200).await;
        let near = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&far.url, &near.url)).await;

        let now = now_unix();
        for route in ["a", "b"] {
            preset_of(&gw, route).deny(window_closed(now + 100), now);
        }

        let mut watching = gw.events().subscribe();
        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!(resp.response.status, 429);

        let event = watching.recv().await.unwrap();
        assert_eq!(event.status, 429);
        assert_eq!(event.model, MODEL);
        assert_eq!(event.ns, NS);
        assert_eq!(
            event.credential,
            crate::stats::NO_CREDENTIAL,
            "答えたのは gateway 自身で、どの credential でもない"
        );
        assert_eq!(
            (far.hits(), near.hits()),
            (0, 0),
            "知らせは流すが、upstream は叩かない"
        );
    }

    /// 上限のヘッダが載っていれば、窓が開く時刻まで締め出す。
    #[tokio::test]
    async fn the_deadline_comes_from_the_rate_limit_headers() {
        let reset = now_unix() + 3600;
        let limited = FakeUpstream::start_with_headers(
            &[
                ("anthropic-ratelimit-unified-7d-status", "rejected"),
                ("anthropic-ratelimit-unified-7d-reset", &reset.to_string()),
                // 窓が読めるなら、こちらは見ない。
                ("retry-after", "5"),
            ],
            |_, _| (429, body_for(429)),
        )
        .await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&limited.url, &spare.url)).await;

        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(
            preset_of(&gw, "a").availability(MODEL, now_unix()),
            Availability::Denied {
                until: reset + denial::RESET_SLACK
            },
            "窓が開く時刻を、少し過ぎるまで"
        );
    }

    /// 期限が過ぎたら、また試す。通ったら印は消える。
    #[tokio::test]
    async fn an_expired_denial_is_tried_again_and_cleared() {
        let recovered = FakeUpstream::always(200).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&recovered.url, &spare.url)).await;

        let past = now_unix() - 1;
        preset_of(&gw, "a").deny(window_closed(past), past);
        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(recovered.hits(), 1, "期限切れの印は素通り");
        assert_eq!(spare.hits(), 0);
        assert_eq!(
            preset_of(&gw, "a").availability(MODEL, past - 100),
            Availability::Ready,
            "通ったので印そのものが消える"
        );
    }

    /// どれも断られているなら、誰にも当てずに開く時刻を返す。
    ///
    /// 開く時刻を知っていながら実リクエストを当てても、429 を貰い直すために
    /// 往復を捨てるだけになる。
    #[tokio::test]
    async fn every_route_denied_is_answered_without_asking() {
        let far = FakeUpstream::always(200).await;
        let near = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&far.url, &near.url)).await;

        let now = now_unix();
        for (route, until) in [("a", now + 1000), ("b", now + 100)] {
            preset_of(&gw, route).deny(window_closed(until), now);
        }

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429);
        assert_eq!(
            resp.response.headers.get("retry-after"),
            Some("100"),
            "最初に開くのがいつかを伝える"
        );
        assert_eq!((far.hits(), near.hits()), (0, 0), "どこにも当てない");
    }

    /// 待っても直らない断り (401 / 403) では締め出さない。
    #[tokio::test]
    async fn an_auth_failure_does_not_start_a_cooldown() {
        for status in [401, 403] {
            let broken = FakeUpstream::always(status).await;
            let spare = FakeUpstream::always(200).await;
            let gw = gateway(&two_credentials(&broken.url, &spare.url)).await;

            gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
                .await
                .unwrap();

            assert_eq!(
                preset_of(&gw, "a").availability(MODEL, now_unix()),
                Availability::Ready,
                "{status} は時間で空くものではない"
            );
        }
    }

    /// 認証情報を要する経路 1 本だけの設定。
    fn oauth_config(url: &str) -> String {
        format!(
            r#"
[credentials.a]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"
url = "{url}"

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
"#
        )
    }

    /// 締め出した経路には裏で枠を聞きに行き、開いていれば戻す。
    ///
    /// 枠は宣言されたリセット時刻より早く開くことがある。実リクエストを
    /// 当てない以上、聞きに行かなければ開いたことに気づけない。聞く先は
    /// 専用の口なので、トークンは 1 つも減らない (DR-0007)。
    #[tokio::test]
    async fn a_limited_route_is_probed_in_the_background() {
        const OPEN: &str = r#"{"limits":[{"kind":"weekly_all","percent":12,"is_active":true,
            "resets_at":null,"scope":null}]}"#;
        let reopened = FakeUpstream::start(|_, req| {
            assert!(
                req.starts_with("GET /api/oauth/usage"),
                "枠を聞くだけで、実弾は撃たない: {req}"
            );
            (200, OPEN.to_owned())
        })
        .await;
        let gw = gateway(&oauth_config(&reopened.url)).await;

        let now = now_unix();
        preset_of(&gw, "a").deny(window_closed(now + 100_000), now);

        let probing = preset_of(&gw, "a").claim_ask(now).expect("札は空いている");
        gw.probe_in_background(&CredentialId::new("a"), preset_of(&gw, "a"), probing)
            .await
            .unwrap();

        assert_eq!(
            preset_of(&gw, "a").availability(MODEL, now),
            Availability::Ready,
            "開いていたので印を外す"
        );
        assert_eq!(reopened.forwards(), 0, "転送は 1 本も走らない");
    }

    /// まだ使い切っていれば、開く時刻を控え直して締め出しを続ける。
    #[tokio::test]
    async fn a_probe_that_is_still_denied_updates_the_deadline() {
        let reset = now_unix() + 7200;
        let iso = format_rfc3339(reset);
        let body = format!(
            r#"{{"limits":[{{"kind":"weekly_all","percent":100,"is_active":true,
               "resets_at":"{iso}","scope":null}}]}}"#
        );
        let still_limited = FakeUpstream::start(move |_, _| (200, body.clone())).await;
        let gw = gateway(&oauth_config(&still_limited.url)).await;

        let now = now_unix();
        preset_of(&gw, "a").deny(window_closed(now + 100), now);

        let probing = preset_of(&gw, "a").claim_ask(now).expect("札は空いている");
        gw.probe_in_background(&CredentialId::new("a"), preset_of(&gw, "a"), probing)
            .await
            .unwrap();

        assert_eq!(
            preset_of(&gw, "a").availability(MODEL, now),
            Availability::Denied {
                until: reset + denial::RESET_SLACK
            },
            "聞いた結果で開く時刻を引き直す"
        );
    }

    /// モデル別の枠を使い切っていれば、そのモデルだけを締め出す。
    ///
    /// 応答ヘッダにはモデル別の枠が出てこないので、この経路でしか作れない印。
    #[tokio::test]
    async fn a_probe_can_close_a_single_family_of_models() {
        let reset = now_unix() + 7200;
        let iso = format_rfc3339(reset);
        let body = format!(
            r#"{{"limits":[
               {{"kind":"weekly_all","percent":30,"is_active":false,"resets_at":"{iso}","scope":null}},
               {{"kind":"weekly_scoped","percent":100,"is_active":false,"resets_at":"{iso}",
                 "scope":{{"model":{{"id":null,"display_name":"Fable"}}}}}}]}}"#
        );
        let up = FakeUpstream::start(move |_, _| (200, body.clone())).await;
        let gw = gateway(&oauth_config(&up.url)).await;

        let now = now_unix();
        let probing = preset_of(&gw, "a")
            .claim_ask(now)
            .expect("印が無くても聞ける");
        gw.probe_in_background(&CredentialId::new("a"), preset_of(&gw, "a"), probing)
            .await
            .unwrap();

        assert!(
            matches!(
                preset_of(&gw, "a").availability("claude-fable-5", now),
                Availability::Denied { .. }
            ),
            "Fable の枠は使い切っている"
        );
        assert_eq!(
            preset_of(&gw, "a").availability("claude-haiku-4-5", now),
            Availability::Ready,
            "他のモデルは巻き込まない"
        );
    }

    /// 理由を言わない 429 を受けたら、その場で枠を聞きに行く。
    ///
    /// 窓を伴わない 429 からは「60 秒ほど空ける」しか決められない。実測
    /// (2026-08-01) ではこの形の 429 が最も多いので、当て推量のまま置かずに
    /// 専用の口へ聞きに行き、どの枠がいつ開くかで置き換える。
    #[tokio::test]
    async fn a_denial_without_a_reason_is_followed_by_a_question() {
        let vague = FakeUpstream::start(|_, req| {
            if req.starts_with("GET /api/oauth/usage") {
                (
                    200,
                    r#"{"limits":[{"kind":"weekly_all","percent":3,"is_active":false,
                       "resets_at":null,"scope":null}]}"#
                        .to_owned(),
                )
            } else {
                // 上限のヘッダを 1 つも載せない 429。
                (429, body_for(429))
            }
        })
        .await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"
url = "{}"

[routes.b]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a", "b"]
"#,
            vague.url, spare.url
        ))
        .await;

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();
        assert_eq!(resp.response.status, 200, "実リクエストは次の経路へ");

        // 転送の 1 本と、その後の問い合わせ。
        vague.next_request().await;
        vague.next_request().await;
        assert!(
            vague
                .requests()
                .iter()
                .any(|req| req.starts_with("GET /api/oauth/usage")),
            "断られた理由を聞きに行く: {:?}",
            vague.requests()
        );
    }

    /// 転送のついでに、締め出している経路へ聞きに行く役を立てる。
    #[tokio::test]
    async fn forwarding_starts_the_probe_for_a_denied_route() {
        let denied = FakeUpstream::always(200).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"
url = "{}"

[routes.b]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a", "b"]
"#,
            denied.url, spare.url
        ))
        .await;

        let now = now_unix();
        preset_of(&gw, "a").deny(window_closed(now + 100_000), now - denial::PROBE_INTERVAL);

        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(spare.hits(), 1, "実リクエストは断られていない方へ");
        denied.next_request().await;
        assert_eq!(denied.hits(), 1, "締め出している方へは裏で聞きに行く");
    }

    /// 枠を聞く口を持たない経路には、様子を聞きに行かない。
    ///
    /// 聞いても今の状態が読めないので、札を取って捨てるより取らない。
    /// 「無い」は型で表されているので、判定に設定の type を見る必要もない。
    #[tokio::test]
    async fn a_route_that_cannot_answer_is_not_probed() {
        let denied = FakeUpstream::always(200).await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&denied.url, &spare.url)).await;

        let now = now_unix();
        preset_of(&gw, "a").deny(window_closed(now + 100_000), now - denial::PROBE_INTERVAL);

        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(denied.hits(), 0, "中継の経路には聞きに行かない");
        assert!(
            preset_of(&gw, "a").claim_probe(now).is_none(),
            "枠照会 API が無いので役自体を引き受けない"
        );
    }

    /// 行き場が無くなったときは、間隔を待たずに聞きに行く。
    ///
    /// 誰も通せないと分かった今が、状態を確かめる価値の最も高い瞬間になる。
    #[tokio::test]
    async fn losing_every_route_asks_right_away() {
        let denied = FakeUpstream::always(200).await;
        let gw = gateway(&oauth_config(&denied.url)).await;

        // 今しがた断られたばかり = 間隔はまったく空いていない。
        let now = now_unix();
        preset_of(&gw, "a").deny(window_closed(now + 100_000), now);

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 429, "実リクエストは当てない");
        denied.next_request().await;
        assert_eq!(denied.hits(), 1, "間隔を待たずに 1 本だけ聞きに行く");
    }

    /// 待たせる長さは、様子を聞きに行く間隔で頭を押さえる。
    ///
    /// 宣言されたリセット時刻より早く開くことがあり、その早期回復に気づく
    /// のは裏で聞きに行った時。2 日後と伝えると、気づいた側から見て嘘になる。
    #[tokio::test]
    async fn the_retry_after_is_capped_at_the_probe_interval() {
        let far = FakeUpstream::always(200).await;
        let also_far = FakeUpstream::always(200).await;
        let gw = gateway(&two_credentials(&far.url, &also_far.url)).await;

        let now = now_unix();
        for route in ["a", "b"] {
            preset_of(&gw, route).deny(window_closed(now + 2 * 24 * 3600), now);
        }

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(
            resp.response.headers.get("retry-after"),
            Some(denial::PROBE_INTERVAL.to_string().as_str())
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

    /// HTTP 200 でも本文先頭が overloaded なら、1 byte も返す前に次の経路へ回す。
    ///
    /// OpenAI Responses API は一時的な混雑を SSE error で返す。status だけで採用すると
    /// fallback の機会を失うため、provider が最初の event を判定して core へ伝える。
    #[tokio::test]
    async fn a_body_level_overload_falls_back_before_streaming() {
        let crowded = FakeUpstream::start_sse(|_, _| {
            (
                200,
                concat!(
                    "data: {\"type\":\"error\",\"error\":{\"message\":",
                    "\"Our servers are currently overloaded. Please try again later.\"}}\n\n",
                    "data: {\"type\":\"response.failed\",\"response\":{}}\n\n"
                )
                .to_owned(),
            )
        })
        .await;
        let spare = FakeUpstream::always(200).await;
        let gw = gateway_with(
            &format!(
                r#"
[credentials.codex]
type = "codex_oauth"

[routes.codex]
provider = "openai"
credential = "codex"
url = "{}/backend-api/codex"
models = ["m"]

[routes.spare]
provider = "anthropic"
url = "{}"
models = ["m"]

[ns.default]
[[ns.default.routing]]
models = ["m"]
routes = ["codex", "spare"]
"#,
                crowded.url, spare.url
            ),
            StaticStore::holding(valid_codex_credential()),
        )
        .await;
        let before = (crowded.hits(), spare.hits());

        let resp = gw
            .forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        assert_eq!(resp.response.status, 200);
        assert!(body_text(resp.response).await.contains("ok"));
        assert_eq!((crowded.hits() - before.0, spare.hits() - before.1), (1, 1));
        assert!(matches!(
            preset_of(&gw, "codex").availability(MODEL, now_unix()),
            Availability::Denied { .. }
        ));
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
[routes.a]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
[routes.first]
provider = "anthropic"
url = "{}"
models = ["m"]

[routes.second]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["first", "second"]
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
[routes.flaky]
provider = "anthropic"
url = "{}"
models = ["m"]

[routes.alive]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["flaky", "alive"]
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
[routes.a]
provider = "anthropic"
url = "{}"
models = ["m"]

[routes.b]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a", "b"]
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

[routes.a]
provider = "anthropic"
credential = "a"
url = "{}"

[routes.b]
provider = "anthropic"
url = "{}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a", "b"]
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
        assert_eq!(negotiating.forwards(), 2, "送り直すのは 1 回だけ");
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
[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["m"]

[[ns.default.routing]]
models = ["known"]
routes = ["a"]
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
[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
[routes.a]
provider = "anthropic"
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

[routes.a]
provider = "anthropic"
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

    /// 使っていない credential も名前は出す。
    ///
    /// 消してしまうと「設定にあるが未観測」と「設定に無い」の区別がつかない。
    #[tokio::test]
    async fn usage_report_lists_every_credential() {
        let gw = gateway(
            r#"
[credentials.claude-personal]
type = "claude_oauth"

[routes.claude-personal]
provider = "anthropic"
credential = "claude-personal"

[credentials.bedrock]
type = "bedrock_api_key"

[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock.invalid/anthropic"

[routes.cpa]
provider = "anthropic"
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
                .all(|c| c.support != crate::quota::Support::Observed),
            "取れない扱いは support の値で分かる"
        );
        assert!(
            report.credentials.iter().all(|c| c.limits.is_none()),
            "聞きに行っていないので枠も無い"
        );
    }

    /// 転送のついでに読めた枠は、そのまま一覧に出る。
    ///
    /// 観測を持つのは経路だが、外から見える形は変わらない (DR-0007)。
    #[tokio::test]
    async fn usage_report_shows_what_the_route_observed() {
        let reset = now_unix() + 3600;
        let up = FakeUpstream::start_with_headers(
            &[
                ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-5h-reset", &reset.to_string()),
            ],
            |_, _| (200, body_for(200)),
        )
        .await;
        let gw = gateway(&oauth_config(&up.url)).await;

        gw.forward(ns(&gw), NS, "/v1/messages", None, request(), vec![])
            .await
            .unwrap();

        let report = gw.usage_report(false).await;
        let entry = &report.credentials[0];
        assert_eq!(entry.support, crate::quota::Support::Observed);
        assert_eq!(
            entry
                .snapshot
                .as_ref()
                .and_then(|s| s.five_hour.as_ref())
                .and_then(|w| w.utilization),
            Some(0.42)
        );
    }

    /// 聞きに行くときは、専用の口から枠も取って一覧に載せる。
    ///
    /// モデル別の枠 (fable など) は応答ヘッダに出てこないので、この口を
    /// 通さないと利用状況の一覧からも見えない。
    #[tokio::test]
    async fn usage_report_carries_the_scoped_limits() {
        const LIMITS: &str = r#"{"limits":[
            {"kind":"weekly_all","percent":100,"severity":"critical",
             "resets_at":"2026-08-02T08:59:59Z","scope":null,"is_active":true},
            {"kind":"weekly_scoped","percent":80,"severity":"warning",
             "resets_at":"2026-08-02T08:59:59Z",
             "scope":{"model":{"id":null,"display_name":"Fable"}},"is_active":false}]}"#;

        let up = FakeUpstream::start(|_, req| {
            if req.starts_with("GET /api/oauth/usage") {
                (200, LIMITS.to_owned())
            } else {
                (200, body_for(200))
            }
        })
        .await;
        let gw = gateway(&format!(
            r#"
[credentials.a]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"
url = "{}"

[routes.relayed]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["m"]

[ns.default]
"#,
            up.url
        ))
        .await;

        let report = gw.usage_report(true).await;
        let subscription = report
            .credentials
            .iter()
            .find(|c| c.name == "a")
            .expect("設定にある");
        let limits = subscription.limits.as_ref().expect("聞けている");

        assert_eq!(limits.len(), 2);
        assert_eq!(limits[1].kind, "weekly_scoped");
        assert_eq!(
            limits[1].model.as_deref(),
            Some("Fable"),
            "どのモデルの枠かが分かる"
        );
        assert_eq!(limits[1].percent, 80.0);

        let other = report
            .credentials
            .iter()
            .find(|c| c.name == "relayed")
            .expect("設定にある");
        assert!(other.limits.is_none(), "聞ける相手でなければ欄ごと出さない");
    }

    /// 非消費 quota API を持つ経路は、推論 probe が無くても枠を取得する。
    #[tokio::test]
    async fn quota_api_without_a_probe_is_still_queried() {
        let up = FakeUpstream::start(|_, request| {
            if request.starts_with("GET /backend-api/wham/usage") {
                (
                    200,
                    r#"{"rate_limit":{"primary_window":{"used_percent":25,"reset_at":1800001000}}}"#
                        .to_owned(),
                )
            } else {
                (200, r#"{"models":[]}"#.to_owned())
            }
        })
        .await;
        let gw = gateway_with(
            &format!(
                r#"
[credentials.codex]
type = "codex_oauth"

[routes.codex]
provider = "openai"
credential = "codex"
url = "{}/backend-api/codex"
models = ["gpt-5.3-codex"]

[ns.default]
"#,
                up.url
            ),
            StaticStore::holding(valid_codex_credential()),
        )
        .await;

        let report = gw.usage_report(true).await;
        let entry = report
            .credentials
            .iter()
            .find(|entry| entry.name == "codex")
            .unwrap();
        assert_eq!(entry.limits.as_ref().unwrap()[0].percent, 25.0);
        assert_eq!(report.probe.unwrap().requests, 0, "推論 token は使わない");
    }

    /// プローブが失敗した credential は理由を載せ、他は返す。
    #[tokio::test]
    async fn a_failing_probe_does_not_take_the_others_down() {
        let gw = gateway(
            r#"
[credentials.nowhere]
type = "claude_oauth"

[routes.nowhere]
provider = "anthropic"
credential = "nowhere"
url = "http://127.0.0.1:9"

[credentials.bedrock]
type = "bedrock_api_key"

[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock.invalid/anthropic"

[ns.default]
"#,
        )
        .await;

        let report = gw.usage_report(true).await;
        let probe = report.probe.expect("投げた記録が残る");
        assert_eq!(probe.requests, 1, "枠を聞ける口を持つのは 1 本だけ");
        assert_eq!(
            probe.model,
            preset_of(&gw, "nowhere")
                .probe_request()
                .expect("枠を聞ける経路")
                .model,
            "何を投げたかは経路が決めた通りに出る"
        );

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

    /// 集計の USD は、その行を出した経路の単価で換算する。
    ///
    /// 認証情報を持たない経路 (`-`) の行にも額が出る。答えた経路の名前が
    /// 記録に残らないだけで、そのモデルに値を付けられる経路はいる (DR-0011)。
    #[tokio::test]
    async fn the_stats_report_prices_each_row_through_its_route() {
        let dir = tempfile::tempdir().unwrap();
        let gw = gateway(&format!(
            r#"
[stats]
dir = "{}"

[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["claude-opus-5"]

[ns.default]
"#,
            dir.path().display()
        ))
        .await;

        let now = now_unix();
        let mut usage = TokenUsage::default();
        usage.set(TokenKind::input(), 1_000_000);
        gw.stats().record(now, Some("a"), "claude-opus-5", &usage);
        gw.stats().record(now, None, "claude-opus-5", &usage);
        // 単価表に無いモデルは、どの経路に聞いても値が付かない。
        gw.stats().record(now, Some("a"), "who-knows", &usage);

        let report = gw.stats_report(7, now);
        let day = report
            .days
            .values()
            .next()
            .expect("記録した日の分が出ている");

        assert_eq!(day.credentials["a"]["claude-opus-5"].usd, Some(5.0));
        assert_eq!(
            day.credentials[crate::stats::NO_CREDENTIAL]["claude-opus-5"].usd,
            Some(5.0),
            "持ち主なしの行も、モデルから経路を辿って換算する"
        );
        assert_eq!(
            day.credentials["a"]["who-knows"].usd, None,
            "推測した額を出さない"
        );
        assert_eq!(day.total_usd, Some(10.0), "値の付く行だけの和");
    }

    /// 短い名前で指定できる。upstream には実際のモデル名で送る。
    #[tokio::test]
    async fn alias_is_resolved_before_forwarding() {
        let up = FakeUpstream::always(200).await;
        let gw = gateway(&format!(
            r#"
[routes.a]
provider = "anthropic"
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
