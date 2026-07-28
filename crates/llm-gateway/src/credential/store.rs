//! 認証情報を使える状態で渡す窓口。
//!
//! 期限が近ければ更新してから返す。同じ認証情報への同時要求は 1 回の更新に
//! 束ね、全員が同じ結果を受け取る。束ねないと、並行リクエストの数だけ更新が
//! 走り、後発が `refresh_token_reused` で弾かれて再ログインが要る状態に落ちる。

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock, broadcast};

use crate::{Error, Result};

use super::time::{format_rfc3339, parse_rfc3339};
use super::{CredentialId, Persistence, StoredCredential, oauth};

/// 期限のこれだけ手前から更新に入る。
///
/// 転送の途中で切れないだけの余裕を取る。長いリクエストでも数分あれば足りる。
const REFRESH_MARGIN_SECS: i64 = 300;

/// upstream に載せる認証情報。
#[derive(Debug, Clone)]
pub struct Credential {
    pub id: CredentialId,
    token: Arc<str>,
    pub account_id: Option<String>,
    /// この upstream が拒否したと分かっている beta フラグ (DR-0003)。
    ///
    /// 取り出した時点で期限を見て絞ってある。期限切れの記録はここに入らず、
    /// 「試してみる」側に回る。
    pub denied_beta: BTreeSet<String>,
}

impl Credential {
    /// `Authorization: Bearer` に載せる値。
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// `x-api-key` に載せる値。
    pub fn api_key(&self) -> &str {
        &self.token
    }

    /// 試験用。store を経由せずに作る。
    #[cfg(test)]
    pub fn for_test(token: &str) -> Self {
        Self {
            id: CredentialId::new("test"),
            token: Arc::from(token),
            account_id: None,
            denied_beta: BTreeSet::new(),
        }
    }
}

/// 更新の結果を待っている側へ配るための合図。
type RefreshSignal = broadcast::Sender<std::result::Result<(), String>>;

pub struct CredentialStore<P: Persistence> {
    persistence: P,
    http: reqwest::Client,
    /// 進行中の更新。同じ id への 2 人目以降はここに相乗りする。
    in_flight: Mutex<HashMap<CredentialId, RefreshSignal>>,
    /// 読み出しのたびにファイルを開かないための控え。
    cache: RwLock<HashMap<CredentialId, Arc<StoredCredential>>>,
    clock: Clock,
    /// 更新先の差し替え口。テストで手元のサーバへ向けるために持つ。
    token_url_override: Option<String>,
}

/// 現在時刻の取り出し口。テストで固定するために挟んである。
#[derive(Clone)]
pub enum Clock {
    System,
    #[cfg(test)]
    Fixed(i64),
}

impl Clock {
    fn now_unix(&self) -> i64 {
        match self {
            Self::System => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            #[cfg(test)]
            Self::Fixed(t) => *t,
        }
    }
}

impl<P: Persistence> CredentialStore<P> {
    pub fn new(persistence: P, http: reqwest::Client) -> Self {
        Self::with_clock(persistence, http, Clock::System)
    }

    fn with_clock(persistence: P, http: reqwest::Client, clock: Clock) -> Self {
        Self {
            persistence,
            http,
            in_flight: Mutex::new(HashMap::new()),
            cache: RwLock::new(HashMap::new()),
            clock,
            token_url_override: None,
        }
    }

    /// 使える認証情報を返す。期限が近ければ更新してから返す。
    pub async fn acquire(&self, id: &CredentialId) -> Result<Credential> {
        let current = self.read(id).await?;

        if !self.needs_refresh(&current) {
            return Ok(self.to_credential(id, &current));
        }

        self.refresh_once(id).await?;

        let refreshed = self.read(id).await?;
        Ok(self.to_credential(id, &refreshed))
    }

    /// upstream が拒否した beta フラグを覚える (DR-0003)。
    ///
    /// 覚えないと同じ 400 を毎回踏む。時刻を一緒に置くのは、upstream が
    /// 対応したときに自動で戻すため。
    pub async fn record_denied_beta(&self, id: &CredentialId, flags: &[String]) -> Result<()> {
        if flags.is_empty() {
            return Ok(());
        }
        let current = self.read(id).await?;
        let mut next = (*current).clone();
        next.record_denied_beta(flags, self.clock.now_unix());

        self.persistence.store(id, &next)?;
        self.cache.write().await.insert(id.clone(), Arc::new(next));
        Ok(())
    }

    /// 現在の内容を返す。控えがあればそれを使う。
    async fn read(&self, id: &CredentialId) -> Result<Arc<StoredCredential>> {
        if let Some(hit) = self.cache.read().await.get(id) {
            return Ok(Arc::clone(hit));
        }
        let loaded = Arc::new(self.persistence.load(id)?);
        self.cache
            .write()
            .await
            .insert(id.clone(), Arc::clone(&loaded));
        Ok(loaded)
    }

    fn needs_refresh(&self, c: &StoredCredential) -> bool {
        // 更新の口が無いもの (API キー認証) は、期限が過ぎていても更新に
        // 走らない。走らせても直らず、失敗の理由が認証情報の期限切れから
        // 「更新できません」にすり替わって原因が見えなくなる。
        if c.payload.oauth_kind().is_none() {
            return false;
        }
        match parse_rfc3339(c.payload.expired()) {
            Some(exp) => exp - self.clock.now_unix() <= REFRESH_MARGIN_SECS,
            // 期限が読めないものは更新しない。壊れた値を根拠に
            // refresh token を使い切るほうが害が大きい。
            None => false,
        }
    }

    /// 更新を 1 回だけ走らせる。同時に来た要求は結果を待つ。
    async fn refresh_once(&self, id: &CredentialId) -> Result<()> {
        // 先着がいれば、その結果を待つ側に回る。
        let leader = {
            let mut in_flight = self.in_flight.lock().await;
            match in_flight.get(id) {
                Some(tx) => {
                    let mut rx = tx.subscribe();
                    drop(in_flight);
                    return match rx.recv().await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(reason)) => Err(Error::Refresh {
                            id: id.to_string(),
                            reason,
                        }),
                        // 先着が結果を配る前に消えた = 更新できたか分からない。
                        Err(_) => Err(Error::Refresh {
                            id: id.to_string(),
                            reason: "更新の結果を受け取れませんでした".to_owned(),
                        }),
                    };
                }
                None => {
                    let (tx, _) = broadcast::channel(1);
                    in_flight.insert(id.clone(), tx.clone());
                    tx
                }
            }
        };

        let outcome = self.do_refresh(id).await;

        // 待っている側へ配ってから、進行中の印を外す。
        self.in_flight.lock().await.remove(id);
        let _ = leader.send(outcome.as_ref().map(|_| ()).map_err(ToString::to_string));
        outcome
    }

    /// 実際の更新。保存まで済ませる。
    async fn do_refresh(&self, id: &CredentialId) -> Result<()> {
        let current = self.read(id).await?;
        let (Some(kind), Some(refresh_token)) = (
            current.payload.oauth_kind(),
            current.payload.refresh_token(),
        ) else {
            return Err(Error::Refresh {
                id: id.to_string(),
                reason: "OAuth ではないので更新できません。\
新しいキーを発行して認証情報を保存し直してください"
                    .to_owned(),
            });
        };

        let resp = oauth::refresh_at(
            &self.http,
            id,
            kind,
            refresh_token,
            self.token_url_override.as_deref(),
        )
        .await?;

        let now = self.clock.now_unix();
        let mut next = (*current).clone();
        if let Some(tokens) = next.payload.oauth_tokens_mut() {
            tokens.access_token = resp.access_token;
            // 返らなかった場合は入れ替わっていないとみなし、今の値を残す。
            if let Some(rt) = resp.refresh_token {
                tokens.refresh_token = rt;
            }
            tokens.expired = format_rfc3339(now + resp.expires_in);
            if let Some(email) = resp.account.and_then(|a| a.email_address) {
                tokens.email = email;
            }
        }
        next.last_refresh = format_rfc3339(now);

        // 保存が先。ここで落ちると新しい token を失うが、控えだけ更新して
        // 保存に失敗するよりはよい (次回起動時に古い token で動こうとして
        // 弾かれ、原因が分からなくなる)。
        self.persistence.store(id, &next)?;
        self.cache.write().await.insert(id.clone(), Arc::new(next));
        Ok(())
    }

    fn to_credential(&self, id: &CredentialId, c: &StoredCredential) -> Credential {
        Credential {
            id: id.clone(),
            token: Arc::from(c.payload.secret()),
            account_id: c.payload.account_id().map(str::to_owned),
            denied_beta: c.denied_beta_at(self.clock.now_unix()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::stored::{ApiKey, OauthTokens, Payload, StoredCredential};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 2026-07-27T19:00:00Z
    const NOW: i64 = 1_785_178_800;

    fn at(offset_secs: i64) -> String {
        format_rfc3339(NOW + offset_secs)
    }

    fn cred(expired: &str) -> StoredCredential {
        StoredCredential::new(Payload::ClaudeOauth(OauthTokens {
            access_token: "at-1".into(),
            refresh_token: "rt-1".into(),
            expired: expired.into(),
            email: "someone@example.com".into(),
            extra: Default::default(),
        }))
    }

    fn api_key_cred(expired: &str) -> StoredCredential {
        StoredCredential::new(Payload::ClaudeBedrock(ApiKey {
            api_key: "ak-1".into(),
            expired: expired.into(),
            extra: Default::default(),
        }))
    }

    /// 保存回数と内容を数えるだけの置き場。
    struct Spy {
        current: StdMutex<StoredCredential>,
        stores: AtomicUsize,
    }

    impl Spy {
        fn new(c: StoredCredential) -> Self {
            Self {
                current: StdMutex::new(c),
                stores: AtomicUsize::new(0),
            }
        }
    }

    impl Persistence for Spy {
        fn load(&self, _id: &CredentialId) -> Result<StoredCredential> {
            Ok(self.current.lock().unwrap().clone())
        }
        fn store(&self, _id: &CredentialId, value: &StoredCredential) -> Result<()> {
            *self.current.lock().unwrap() = value.clone();
            self.stores.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn list(&self) -> Result<Vec<CredentialId>> {
            Ok(vec![CredentialId::new("c")])
        }
    }

    fn store_with(c: StoredCredential) -> CredentialStore<Spy> {
        CredentialStore::with_clock(Spy::new(c), reqwest::Client::new(), Clock::Fixed(NOW))
    }

    /// 更新要求を数える試験用サーバ。
    ///
    /// 応答を少し遅らせる。即答すると 1 本目が終わってから 2 本目が来る形に
    /// なりやすく、束ねられているのか単に直列なのか区別がつかない。
    struct FakeTokenServer {
        url: String,
        hits: Arc<AtomicUsize>,
    }

    impl FakeTokenServer {
        async fn start(delay: std::time::Duration) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));

            let counter = Arc::clone(&hits);
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let counter = Arc::clone(&counter);
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

                        let mut buf = vec![0u8; 8192];
                        let _ = sock.read(&mut buf).await;
                        counter.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(delay).await;

                        let n = counter.load(Ordering::SeqCst);
                        let body = format!(
                            r#"{{"access_token":"at-{n}","refresh_token":"rt-{n}","expires_in":28800}}"#
                        );
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.flush().await;
                    });
                }
            });

            Self {
                url: format!("http://{addr}/oauth/token"),
                hits,
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    async fn store_against(c: StoredCredential, server: &FakeTokenServer) -> CredentialStore<Spy> {
        let mut store =
            CredentialStore::with_clock(Spy::new(c), reqwest::Client::new(), Clock::Fixed(NOW));
        store.token_url_override = Some(server.url.clone());
        store
    }

    #[tokio::test]
    async fn valid_token_is_returned_as_is() {
        let store = store_with(cred(&at(3600)));
        let got = store.acquire(&CredentialId::new("c")).await.unwrap();

        assert_eq!(got.bearer(), "Bearer at-1");
        assert_eq!(
            store.persistence.stores.load(Ordering::SeqCst),
            0,
            "まだ有効なら更新しない"
        );
    }

    /// 期限まで余裕があるかどうかの境目。
    #[test]
    fn refresh_margin() {
        let store = store_with(cred(""));
        assert!(!store.needs_refresh(&cred(&at(REFRESH_MARGIN_SECS + 1))));
        assert!(store.needs_refresh(&cred(&at(REFRESH_MARGIN_SECS))));
        assert!(store.needs_refresh(&cred(&at(0))), "期限ちょうど");
        assert!(store.needs_refresh(&cred(&at(-1))), "切れている");
    }

    /// 期限が壊れていても更新に走らない。refresh token を無駄に使わない。
    #[test]
    fn unreadable_expiry_does_not_trigger_refresh() {
        let store = store_with(cred(""));
        for bad in ["", "not-a-date", "2026-07", "yesterday"] {
            assert!(!store.needs_refresh(&cred(bad)), "{bad:?}");
        }
    }

    /// 更新の口が無い認証情報は、期限が切れていても更新に走らない。
    ///
    /// 走らせても直らないうえ、失敗の理由が「キーの期限切れ」から
    /// 「更新できません」にすり替わって原因が見えなくなる。
    #[test]
    fn api_key_is_never_refreshed() {
        let store = store_with(api_key_cred(&at(-1)));
        assert!(!store.needs_refresh(&api_key_cred(&at(-1))));
        assert!(!store.needs_refresh(&api_key_cred(&at(0))));
    }

    /// それでも更新を頼まれたら、何をすればよいかを言って断る。
    #[tokio::test]
    async fn refreshing_an_api_key_says_what_to_do() {
        let store = store_with(api_key_cred(&at(-1)));
        let err = store
            .do_refresh(&CredentialId::new("c"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("OAuth ではない"), "{err}");
        assert!(err.contains("発行"), "対処を書く: {err}");
    }

    /// API キーはそのまま渡る (期限を理由に握りつぶさない)。
    #[tokio::test]
    async fn api_key_is_handed_out_as_is() {
        let store = store_with(api_key_cred(&at(-1)));
        let got = store.acquire(&CredentialId::new("c")).await.unwrap();
        assert_eq!(got.api_key(), "ak-1");
        assert_eq!(store.persistence.stores.load(Ordering::SeqCst), 0);
    }

    /// 拒否された beta フラグを覚えて保存する。
    #[tokio::test]
    async fn denied_beta_is_recorded_and_persisted() {
        let store = store_with(cred(&at(3600)));
        let id = CredentialId::new("c");

        store
            .record_denied_beta(&id, &["advisor-tool-2026-03-01".to_owned()])
            .await
            .unwrap();

        let saved = store.persistence.current.lock().unwrap().clone();
        assert_eq!(
            saved.denied_beta.get("advisor-tool-2026-03-01").unwrap(),
            &format_rfc3339(NOW),
            "確認した時刻を一緒に残す"
        );
        assert_eq!(store.persistence.stores.load(Ordering::SeqCst), 1);
    }

    /// 覚えることが無ければ書かない (無駄な書き込みで競合を増やさない)。
    #[tokio::test]
    async fn recording_nothing_does_not_write() {
        let store = store_with(cred(&at(3600)));
        store
            .record_denied_beta(&CredentialId::new("c"), &[])
            .await
            .unwrap();
        assert_eq!(store.persistence.stores.load(Ordering::SeqCst), 0);
    }

    /// 取り出した認証情報には、期限内の拒否リストだけが乗る。
    #[tokio::test]
    async fn acquire_carries_live_denials_only() {
        let mut c = cred(&at(3600));
        c.record_denied_beta(&["fresh".to_owned()], NOW - 3600);
        c.record_denied_beta(&["old".to_owned()], NOW - 86_400 * 2);

        let store = store_with(c);
        let got = store.acquire(&CredentialId::new("c")).await.unwrap();

        assert!(got.denied_beta.contains("fresh"));
        assert!(!got.denied_beta.contains("old"), "期限切れは試してみる側");
    }

    /// 同時に来た要求が更新を重ねない。
    ///
    /// 重ねると後発が refresh_token_reused で弾かれ、再ログインが要る。
    /// ここでは更新先へ実際に繋がないので全員が失敗するが、
    /// **保存が 1 回も走らない = 更新自体が 1 本に束ねられた**ことを見る。
    #[tokio::test]
    async fn concurrent_acquires_share_one_refresh() {
        let store = Arc::new(store_with(cred(&at(-1))));
        let id = CredentialId::new("c");

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let store = Arc::clone(&store);
            let id = id.clone();
            tasks.push(tokio::spawn(async move { store.acquire(&id).await }));
        }

        let mut failures = 0;
        for t in tasks {
            if t.await.unwrap().is_err() {
                failures += 1;
            }
        }

        assert_eq!(failures, 8, "更新先に繋がらないので全員失敗する");
        assert!(
            store.in_flight.lock().await.is_empty(),
            "進行中の印が残っていると次回以降が永久に待つ"
        );
    }

    /// 期限切れなら更新し、新しい token を返して保存する。
    #[tokio::test]
    async fn expired_token_is_refreshed_and_persisted() {
        let server = FakeTokenServer::start(std::time::Duration::ZERO).await;
        let store = store_against(cred(&at(-1)), &server).await;
        let id = CredentialId::new("c");

        let got = store.acquire(&id).await.unwrap();

        assert_eq!(got.bearer(), "Bearer at-1", "新しい token が返る");
        assert_eq!(server.hits(), 1);
        assert_eq!(store.persistence.stores.load(Ordering::SeqCst), 1);

        let saved = store.persistence.current.lock().unwrap().clone();
        assert_eq!(saved.payload.secret(), "at-1");
        assert_eq!(
            saved.payload.refresh_token(),
            Some("rt-1"),
            "入れ替わった refresh token を保存する"
        );
        assert_eq!(
            saved.payload.expired(),
            format_rfc3339(NOW + 28_800),
            "期限は expires_in から引き直す"
        );
        assert_eq!(saved.last_refresh, format_rfc3339(NOW));
    }

    /// 同時に来た 8 本が 1 回の更新に束ねられ、全員が同じ token を得る。
    ///
    /// ここが崩れると、2 本目以降が使用済みの refresh token を送って
    /// 弾かれ、再ログインが要る状態に落ちる。
    #[tokio::test]
    async fn concurrent_acquires_trigger_exactly_one_refresh() {
        let server = FakeTokenServer::start(std::time::Duration::from_millis(50)).await;
        let store = Arc::new(store_against(cred(&at(-1)), &server).await);
        let id = CredentialId::new("c");

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let store = Arc::clone(&store);
            let id = id.clone();
            tasks.push(tokio::spawn(async move { store.acquire(&id).await }));
        }

        let mut tokens = Vec::new();
        for t in tasks {
            tokens.push(t.await.unwrap().expect("全員成功する").bearer());
        }

        assert_eq!(server.hits(), 1, "更新先を叩くのは 1 回だけ");
        assert_eq!(
            store.persistence.stores.load(Ordering::SeqCst),
            1,
            "保存も 1 回だけ"
        );
        assert!(
            tokens.iter().all(|t| *t == tokens[0]),
            "全員が同じ token を受け取る: {tokens:?}"
        );
    }

    /// 更新が済んだ後の要求は、もう更新に入らない。
    #[tokio::test]
    async fn refreshed_credential_is_reused() {
        let server = FakeTokenServer::start(std::time::Duration::ZERO).await;
        let store = store_against(cred(&at(-1)), &server).await;
        let id = CredentialId::new("c");

        let first = store.acquire(&id).await.unwrap();
        let second = store.acquire(&id).await.unwrap();

        assert_eq!(first.bearer(), second.bearer());
        assert_eq!(server.hits(), 1, "2 回目は更新しない");
    }

    /// 失敗しても進行中の印は消える (消えないと以後の更新が止まる)。
    #[tokio::test]
    async fn in_flight_marker_is_cleared_after_failure() {
        let store = store_with(cred(&at(-1)));
        let id = CredentialId::new("c");

        assert!(store.acquire(&id).await.is_err());
        assert!(store.in_flight.lock().await.is_empty());

        // 2 回目も同じように失敗する (待ちっぱなしにならない)。
        assert!(store.acquire(&id).await.is_err());
    }
}
