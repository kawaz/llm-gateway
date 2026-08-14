//! 認証情報を使える状態で渡す窓口。
//!
//! 期限が近ければ更新してから返す。同じ認証情報への同時要求は 1 回の更新に
//! 束ね、全員が同じ結果を受け取る。束ねないと、並行リクエストの数だけ更新が
//! 走り、後発が `refresh_token_reused` で弾かれて再ログインが要る状態に落ちる。
//!
//! 置き場は他のプロセスとも共有しているので、束ねるだけでは足りない。書き換え
//! の間は置き場のロックで締め出し、読み出しでは控えの版を照合して相手の書き
//! 込みに気づく (DR-0010)。

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{RwLock, broadcast};

use crate::{Error, Result};

use super::time::{format_rfc3339, parse_rfc3339};
use super::{CredentialId, Payload, Persistence, StoredCredential, oauth};

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

/// 待っている側へ配る言葉。
///
/// 更新の失敗は理由だけを渡す。丸ごと渡すと、受け取った側がもう一度
/// [`Error::Refresh`] に包んで「更新に失敗しました: 更新に失敗しました: …」に
/// なる。
fn reason_of(e: &Error) -> String {
    match e {
        Error::Refresh { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

/// 控えの 1 件。読んだ時点の版を一緒に持つ。
///
/// 版を持たないと、他のプロセスが書いた結果に期限切れまで気づけない。
#[derive(Clone)]
struct Cached {
    value: Arc<StoredCredential>,
    version: Option<u64>,
}

pub struct CredentialStore<P: Persistence> {
    inner: Arc<Inner<P>>,
}

/// 中身は共有されているので、複製しても同じ控え・同じ進行中の印を見る。
///
/// 要求から切り離した仕事 (裏で様子を聞きに行く等) へ持ち出すために要る。
impl<P: Persistence> Clone for CredentialStore<P> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// 共有される中身。
///
/// 更新は要求とは切り離した仕事として走らせるので、控えも進行中の印も
/// 「誰か 1 人のもの」にできない。まとめて抱えて渡す。
struct Inner<P: Persistence> {
    persistence: P,
    http: reqwest::Client,
    /// 進行中の更新。同じ id への 2 人目以降はここに相乗りする。
    ///
    /// 待たない Mutex なのは、印を外すのが [`RefreshHandoff`] の [`Drop`] で、
    /// そこで await できないため。持っている間にするのは印の出し入れだけで、
    /// 待ちを挟まないので待たない錠で足りる。
    in_flight: Mutex<HashMap<CredentialId, RefreshSignal>>,
    /// 読み出しのたびにファイルを開かないための控え。
    cache: RwLock<HashMap<CredentialId, Cached>>,
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
        Self::with_clock(persistence, http, Clock::System, None)
    }

    fn with_clock(
        persistence: P,
        http: reqwest::Client,
        clock: Clock,
        token_url_override: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                persistence,
                http,
                in_flight: Mutex::new(HashMap::new()),
                cache: RwLock::new(HashMap::new()),
                clock,
                token_url_override,
            }),
        }
    }

    /// 使える認証情報を返す。期限が近ければ更新してから返す。
    pub async fn acquire(&self, id: &CredentialId) -> Result<Credential> {
        self.inner.acquire(id).await
    }

    /// upstream が拒否した beta フラグを覚える (DR-0003)。
    pub async fn record_denied_beta(&self, id: &CredentialId, flags: &[String]) -> Result<()> {
        self.inner.record_denied_beta(id, flags).await
    }
}

impl<P: Persistence> Inner<P> {
    async fn acquire(self: &Arc<Self>, id: &CredentialId) -> Result<Credential> {
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
    async fn record_denied_beta(
        self: &Arc<Self>,
        id: &CredentialId,
        flags: &[String],
    ) -> Result<()> {
        if flags.is_empty() {
            return Ok(());
        }
        // 読んで直して書くまでの間、他のプロセスを締め出す。挟まれると、
        // 相手が書いた更新を古い土台で上書きして消す。
        let _guard = self.lock(id).await?;

        // 控えではなく置き場から積み直す。控えを土台にすると、別のプロセスが
        // 更新した token を古い値で上書きして消す。
        let current = self.reload(id).await?;
        let mut next = (*current).clone();
        next.record_denied_beta(flags, self.clock.now_unix());

        self.persistence.store(id, &next)?;
        self.remember(id, next).await;
        Ok(())
    }

    /// 書き換えの権利を取る。手放すのは戻り値を落としたとき。
    ///
    /// 取れるまでの待ちはブロックするので、専用のスレッドへ逃がす。待ちの
    /// 間は寝ていて、相手が手放した時点で起きる (様子を見に行かない)。
    async fn lock(self: &Arc<Self>, id: &CredentialId) -> Result<P::Guard> {
        let me = Arc::clone(self);
        let owned = id.clone();
        tokio::task::spawn_blocking(move || me.persistence.lock(&owned))
            .await
            .map_err(|e| Error::Credential {
                id: id.to_string(),
                reason: format!("could not wait for the credential lock: {e}"),
            })?
    }

    /// 現在の内容を返す。控えが今の版のままならそれを使う。
    async fn read(&self, id: &CredentialId) -> Result<Arc<StoredCredential>> {
        if let Some(hit) = self.cache.read().await.get(id)
            && hit.version == self.persistence.version(id)
        {
            return Ok(Arc::clone(&hit.value));
        }
        self.reload(id).await
    }

    /// 置き場から読み直し、控えを入れ替える。
    ///
    /// 同じ置き場を複数のプロセスが共有しているので、控えだけを見ていると
    /// 他のプロセスが書いた結果に気づけない。書き込みの前と、refresh token を
    /// 使う前は、ここを通って最新を掴む。
    async fn reload(&self, id: &CredentialId) -> Result<Arc<StoredCredential>> {
        // 版を先に見る。読んだ後に見ると、読み終えてから書かれた中身を
        // 「今の版」として覚え、その更新に気づけなくなる。逆の順なら、
        // 取りこぼしても次の読み出しで版が食い違って読み直しになる。
        let version = self.persistence.version(id);
        let value = Arc::new(self.persistence.load(id)?);
        self.cache.write().await.insert(
            id.clone(),
            Cached {
                value: Arc::clone(&value),
                version,
            },
        );
        Ok(value)
    }

    /// 自分が書いた内容を控えに載せる。
    ///
    /// 版は書いた後に読む。書き換えの権利を持っている間しか呼ばないので、
    /// この間に他のプロセスが割り込むことはない。
    async fn remember(&self, id: &CredentialId, value: StoredCredential) {
        let version = self.persistence.version(id);
        self.cache.write().await.insert(
            id.clone(),
            Cached {
                value: Arc::new(value),
                version,
            },
        );
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

    /// 進行中の印を開く。
    ///
    /// 毒 (持ち手が panic した印) は無視して中身を使う。入っているのは印だけ
    /// で、途中まで書き換えた不整合な状態にならない。加えてここを開くのは
    /// 後始末の [`Drop`] でもあり、そこで panic すると process ごと落ちる。
    fn in_flight(&self) -> MutexGuard<'_, HashMap<CredentialId, RefreshSignal>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 更新を 1 回だけ走らせ、その結果を待つ。
    ///
    /// 更新そのものは要求から切り離した仕事として走らせる。要求は途中で
    /// 消える (クライアントが切る、上位が諦める) が、更新は途中で消えては
    /// 困る — refresh token は 1 回しか使えないので、送った後に投げ出すと
    /// 結果を受け取れないまま焼いたことになる。進行中の印を外すのも
    /// 切り離した側なので、要求が消えても後続が待ちっぱなしにならない。
    async fn refresh_once(self: &Arc<Self>, id: &CredentialId) -> Result<()> {
        let mut result = {
            let mut in_flight = self.in_flight();
            match in_flight.get(id) {
                // 先着がいれば、その結果を待つ側に回る。
                Some(tx) => tx.subscribe(),
                None => {
                    let (tx, rx) = broadcast::channel(1);
                    in_flight.insert(id.clone(), tx.clone());

                    let me = Arc::clone(self);
                    let owned = id.clone();
                    tokio::spawn(async move {
                        // 印を外して結果を配るのは handoff に任せる。仕事が
                        // 途中で落ちても必ず通る道にしておかないと、待って
                        // いる側が来ない合図を待ち続ける。
                        let mut handoff = RefreshHandoff::new(Arc::clone(&me), owned.clone(), tx);
                        let outcome = me.do_refresh(&owned).await;
                        handoff.finish(outcome.as_ref().map(|_| ()).map_err(reason_of));
                    });
                    rx
                }
            }
        };

        match result.recv().await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(Error::Refresh {
                id: id.to_string(),
                reason,
            }),
            // 結果が配られる前に消えた = 更新できたか分からない。
            Err(_) => Err(Error::Refresh {
                id: id.to_string(),
                reason: "did not receive the refresh result".to_owned(),
            }),
        }
    }

    /// 実際の更新。保存まで済ませる。
    ///
    /// 控えではなく置き場から読み直してから入る。refresh token は 1 回しか
    /// 使えないので、控えを信じて走ると、同じ置き場を共有する別のプロセスが
    /// 既に使い切った値を送ることになる。
    async fn do_refresh(self: &Arc<Self>, id: &CredentialId) -> Result<()> {
        // 読み直しから保存までを丸ごと締め出す。同じ置き場を使う別のプロセスは
        // ここで待たされ、権利を得た時点の読み直しで「相手が済ませた」と分かり、
        // 更新に入らずに済む。束ねているのは同じプロセスの中だけなので、
        // ここに来るのは 1 プロセスにつき 1 本。
        let _guard = self.lock(id).await?;

        let current = self.reload(id).await?;
        let (Some(kind), Some(refresh_token)) = (
            current.payload.oauth_kind(),
            current.payload.refresh_token(),
        ) else {
            return Err(Error::Refresh {
                id: id.to_string(),
                reason: "cannot refresh a non-OAuth credential. \
Issue a new key and save the credential again"
                    .to_owned(),
            });
        };

        // 読み直した時点で期限に余裕があるなら、別のプロセスが更新を済ませて
        // いる。ここで走らせても、有効な refresh token を 1 つ捨てるだけ。
        if !self.needs_refresh(&current) {
            return Ok(());
        }

        let resp = match oauth::refresh_at(
            &self.http,
            id,
            kind,
            refresh_token,
            self.token_url_override.as_deref(),
        )
        .await
        {
            Ok(resp) => resp,
            // 断られた理由が「別のプロセスが先に使った」なら、その結果はもう
            // 置き場にある。拾えたら回復し、拾えなければ元の理由を返す。
            //
            // Design rationale: 失敗の種別で振り分けていない。読み直して有効
            // なら成功、古いままなら失敗、という判定は理由に依らず正しく、
            // 種別を見分けるには理由の文字列を当てにするしかないため。
            Err(e) => {
                // 読み直せなかったときも断られた理由を返す。ここを `?` にすると
                // 原因が「更新を断られた」から「置き場を読めない」にすり替わる。
                let Ok(latest) = self.reload(id).await else {
                    return Err(e);
                };
                if self.needs_refresh(&latest) {
                    return Err(e);
                }
                return Ok(());
            }
        };

        let now = self.clock.now_unix();
        let mut next = (*current).clone();
        apply_refresh(&mut next, resp, now);

        // 保存が先。ここで落ちると新しい token を失うが、控えだけ更新して
        // 保存に失敗するよりはよい (次回起動時に古い token で動こうとして
        // 弾かれ、原因が分からなくなる)。
        self.persistence.store(id, &next)?;
        self.remember(id, next).await;
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

fn apply_refresh(credential: &mut StoredCredential, response: oauth::RefreshResponse, now: i64) {
    if let Some(tokens) = credential.payload.oauth_tokens_mut() {
        if let Some(access_token) = response.access_token {
            tokens.access_token = access_token;
        }
        if let Some(refresh_token) = response.refresh_token {
            tokens.refresh_token = refresh_token;
        }
        if let Some(expires_in) = response.expires_in {
            tokens.expired = format_rfc3339(now + expires_in);
        }
        if let Some(email) = response.account.and_then(|account| account.email_address) {
            tokens.email = email;
        }
    }
    if let Some(id_token) = response.id_token
        && let Payload::CodexOauth(tokens) = &mut credential.payload
    {
        tokens.account_id = oauth::account_id_from_token(&id_token).or(tokens.account_id.take());
        tokens.id_token = Some(id_token);
    }
    credential.last_refresh = format_rfc3339(now);
}

/// 切り離した更新の後始末。落ちるときに印を外して結果を配る。
///
/// 走り切った経路だけで後始末をすると、途中で panic した場合に印が残り、
/// その認証情報を求めた全員が来ない合図を待ち続ける (process を入れ替える
/// まで戻らない)。[`Drop`] に寄せておけば、走り切っても落ちても同じ道を通る。
struct RefreshHandoff<P: Persistence> {
    inner: Arc<Inner<P>>,
    id: CredentialId,
    tx: RefreshSignal,
    /// 走り切った結果。無いまま落ちたら、途中で途切れたということ。
    outcome: Option<std::result::Result<(), String>>,
}

impl<P: Persistence> RefreshHandoff<P> {
    fn new(inner: Arc<Inner<P>>, id: CredentialId, tx: RefreshSignal) -> Self {
        Self {
            inner,
            id,
            tx,
            outcome: None,
        }
    }

    /// 走り切った結果を預ける。配るのは落ちるとき。
    fn finish(&mut self, outcome: std::result::Result<(), String>) {
        self.outcome = Some(outcome);
    }
}

impl<P: Persistence> Drop for RefreshHandoff<P> {
    fn drop(&mut self) {
        // 印を外してから配る。逆にすると、起きた側が残った印を見て次の
        // 更新に入れなくなる。
        self.inner.in_flight().remove(&self.id);

        // 途切れた理由は追わない。待っている側にできるのは「もう一度頼む」
        // だけなので、panic の中身を運んでも打つ手は変わらない。
        let outcome = self
            .outcome
            .take()
            .unwrap_or_else(|| Err("the refresh task ended unexpectedly".to_owned()));
        let _ = self.tx.send(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::file::FileStore;
    use crate::credential::stored::{ApiKey, CodexTokens, OauthTokens, Payload, StoredCredential};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 2026-07-27T19:00:00Z
    const NOW: i64 = 1_785_178_800;

    fn at(offset_secs: i64) -> String {
        format_rfc3339(NOW + offset_secs)
    }

    fn cred(expired: &str) -> StoredCredential {
        cred_with("at-1", expired)
    }

    fn cred_with(access_token: &str, expired: &str) -> StoredCredential {
        StoredCredential::new(Payload::ClaudeOauth(OauthTokens {
            access_token: access_token.into(),
            refresh_token: "rt-1".into(),
            expired: expired.into(),
            email: "someone@example.com".into(),
            extra: Default::default(),
        }))
    }

    fn api_key_cred(expired: &str) -> StoredCredential {
        StoredCredential::new(Payload::BedrockApiKey(ApiKey {
            api_key: "ak-1".into(),
            expired: expired.into(),
            extra: Default::default(),
        }))
    }

    /// ChatGPT refresh は部分応答なので、返らない項目を保存済みの値から消さない。
    #[test]
    fn partial_codex_refresh_updates_only_returned_fields() {
        let mut credential = StoredCredential::new(Payload::CodexOauth(CodexTokens {
            oauth: OauthTokens {
                access_token: "at-1".into(),
                refresh_token: "rt-1".into(),
                expired: at(60),
                email: "before@example.com".into(),
                extra: Default::default(),
            },
            id_token: Some("id-1".into()),
            account_id: Some("acc-1".into()),
        }));
        apply_refresh(
            &mut credential,
            oauth::RefreshResponse {
                access_token: Some("at-2".into()),
                refresh_token: None,
                id_token: None,
                expires_in: None,
                account: None,
            },
            NOW,
        );

        let Payload::CodexOauth(tokens) = credential.payload else {
            panic!("Codex のまま")
        };
        assert_eq!(tokens.oauth.access_token, "at-2");
        assert_eq!(tokens.oauth.refresh_token, "rt-1");
        assert_eq!(tokens.oauth.expired, at(60));
        assert_eq!(tokens.oauth.email, "before@example.com");
        assert_eq!(tokens.id_token.as_deref(), Some("id-1"));
        assert_eq!(tokens.account_id.as_deref(), Some("acc-1"));
    }

    /// 保存回数と内容を数えるだけの置き場。
    ///
    /// [`Spy::swapping`] を使うと、途中で内容が入れ替わる置き場になる
    /// (= 同じ置き場を共有する別のプロセスが書いた状況)。
    struct Spy {
        current: StdMutex<StoredCredential>,
        stores: AtomicUsize,
        loads: AtomicUsize,
        /// 何回目の読み出しで、何に入れ替わるか。
        swap: StdMutex<Option<(usize, StoredCredential)>>,
        /// 何回目の読み出しで落ちるか (0 = 落ちない)。
        panic_at: AtomicUsize,
        /// 中身が入れ替わるたびに進む版。
        version: AtomicUsize,
        /// 書き換えの権利。プロセスをまたがないので、ただの Mutex で足りる。
        guard: Arc<tokio::sync::Mutex<()>>,
    }

    impl Spy {
        fn new(c: StoredCredential) -> Self {
            Self {
                current: StdMutex::new(c),
                stores: AtomicUsize::new(0),
                loads: AtomicUsize::new(0),
                swap: StdMutex::new(None),
                panic_at: AtomicUsize::new(0),
                version: AtomicUsize::new(0),
                guard: Arc::new(tokio::sync::Mutex::new(())),
            }
        }

        /// `at` 回目の読み出しの直前に、別のプロセスが `next` を書いた形にする。
        fn swapping(self, at: usize, next: StoredCredential) -> Self {
            *self.swap.lock().unwrap() = Some((at, next));
            self
        }

        /// `at` 回目の読み出しで落ちる置き場にする。
        ///
        /// 更新の経路に自前の unwrap は無いので、panic は置き場側から起こす。
        fn panicking(self, at: usize) -> Self {
            self.panic_at.store(at, Ordering::SeqCst);
            self
        }
    }

    impl Persistence for Spy {
        type Guard = tokio::sync::OwnedMutexGuard<()>;

        fn load(&self, _id: &CredentialId) -> Result<StoredCredential> {
            let n = self.loads.fetch_add(1, Ordering::SeqCst) + 1;
            assert!(
                n != self.panic_at.load(Ordering::SeqCst),
                "試験用: {n} 回目の読み出しで落ちる"
            );
            let mut swap = self.swap.lock().unwrap();
            if let Some((at, next)) = swap.as_ref()
                && n >= *at
            {
                *self.current.lock().unwrap() = next.clone();
                *swap = None;
                self.version.fetch_add(1, Ordering::SeqCst);
            }
            Ok(self.current.lock().unwrap().clone())
        }
        fn store(&self, _id: &CredentialId, value: &StoredCredential) -> Result<()> {
            *self.current.lock().unwrap() = value.clone();
            self.stores.fetch_add(1, Ordering::SeqCst);
            self.version.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn list(&self) -> Result<Vec<CredentialId>> {
            Ok(vec![CredentialId::new("c")])
        }
        fn lock(&self, _id: &CredentialId) -> Result<Self::Guard> {
            Ok(Arc::clone(&self.guard).blocking_lock_owned())
        }
        fn version(&self, _id: &CredentialId) -> Option<u64> {
            Some(self.version.load(Ordering::SeqCst) as u64)
        }
    }

    fn store_with(c: StoredCredential) -> CredentialStore<Spy> {
        store_sharing(Spy::new(c))
    }

    fn store_sharing(disk: Spy) -> CredentialStore<Spy> {
        process_over(disk, None)
    }

    /// 更新要求を数える試験用サーバ。
    ///
    /// 応答を少し遅らせる。即答すると 1 本目が終わってから 2 本目が来る形に
    /// なりやすく、束ねられているのか単に直列なのか区別がつかない。
    struct FakeTokenServer {
        url: String,
        hits: Arc<AtomicUsize>,
        /// 要求が届いた合図。届いた時点を掴めないと、更新の最中に何かを
        /// させる試験が「たぶんこの辺」になる。
        arrived: Arc<tokio::sync::Notify>,
        hold: Hold,
    }

    /// 応答をいつ返すか。
    #[derive(Clone)]
    enum Hold {
        /// 決めた時間だけ待つ。
        For(std::time::Duration),
        /// [`FakeTokenServer::release`] で開くまで待つ。
        Until(Arc<tokio::sync::watch::Sender<bool>>),
    }

    impl Hold {
        async fn wait(&self) {
            match self {
                Self::For(d) => tokio::time::sleep(*d).await,
                // 一度開いたら開きっぱなし。1 本だけ通す作りにすると、
                // 締め出しが壊れて要求が 2 本来たときに試験が落ちずに
                // 止まってしまい、原因が見えない。
                Self::Until(gate) => {
                    let mut open = gate.subscribe();
                    let _ = open.wait_for(|open| *open).await;
                }
            }
        }
    }

    impl FakeTokenServer {
        async fn start(delay: std::time::Duration) -> Self {
            Self::start_with(Hold::For(delay), false).await
        }

        /// 応答を握ったまま待つサーバ。
        ///
        /// 更新の最中に相手が何をしているかを見てから先へ進めたいときに使う。
        /// 時間で待つと「間に合ったから通った」試験になり、締め出しが効いて
        /// いるのか単に速かったのか区別がつかない。
        async fn start_gated() -> Self {
            let gate = Arc::new(tokio::sync::watch::Sender::new(false));
            Self::start_with(Hold::Until(gate), false).await
        }

        /// 更新を断るサーバ。refresh token が既に使われていた状況。
        async fn start_rejecting() -> Self {
            Self::start_with(Hold::For(std::time::Duration::ZERO), true).await
        }

        async fn start_with(hold: Hold, reject: bool) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let arrived = Arc::new(tokio::sync::Notify::new());

            let counter = Arc::clone(&hits);
            let bell = Arc::clone(&arrived);
            let holding = hold.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let counter = Arc::clone(&counter);
                    let bell = Arc::clone(&bell);
                    let holding = holding.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

                        let mut buf = vec![0u8; 8192];
                        let _ = sock.read(&mut buf).await;
                        counter.fetch_add(1, Ordering::SeqCst);
                        bell.notify_one();
                        holding.wait().await;

                        let n = counter.load(Ordering::SeqCst);
                        let (status, body) = if reject {
                            ("400 Bad Request", r#"{"error":"invalid_grant"}"#.to_owned())
                        } else {
                            (
                                "200 OK",
                                format!(
                                    r#"{{"access_token":"at-{n}","refresh_token":"rt-{n}","expires_in":28800}}"#
                                ),
                            )
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
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
                arrived,
                hold,
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        /// 要求が 1 本届くまで待つ。届いた時点で、更新を始めた側は
        /// 応答待ちに入っている。
        async fn wait_for_hit(&self) {
            self.arrived.notified().await;
        }

        /// 握っていた応答を返す。以後の要求も待たせない。
        fn release(&self) {
            match &self.hold {
                Hold::Until(gate) => {
                    gate.send_replace(true);
                }
                Hold::For(_) => panic!("時間で待つサーバに release は無い"),
            }
        }
    }

    async fn store_against(disk: Spy, server: &FakeTokenServer) -> CredentialStore<Spy> {
        process_over(disk, Some(server))
    }

    /// 同じディレクトリを見る別プロセスぶんの窓口。
    ///
    /// flock は同じプロセスの別の fd どうしでも競合するので、置き場を
    /// 開き直せば「もう 1 つのプロセス」として通る。
    fn another_process(
        dir: &std::path::Path,
        server: Option<&FakeTokenServer>,
    ) -> CredentialStore<FileStore> {
        process_over(FileStore::open(dir).unwrap(), server)
    }

    fn process_over<P: Persistence>(
        disk: P,
        server: Option<&FakeTokenServer>,
    ) -> CredentialStore<P> {
        CredentialStore::with_clock(
            disk,
            reqwest::Client::new(),
            Clock::Fixed(NOW),
            server.map(|s| s.url.clone()),
        )
    }

    /// 置き場の出入りを共有の帳面に残す包み。
    ///
    /// 結果だけを見る試験は、締め出しが効いたのか単にすれ違わなかったのかを
    /// 区別できない。誰がいつ待たされ、いつ掴み、いつ書いたかを順に残して
    /// おけば、順序そのものを確かめられる。
    struct Watched {
        inner: FileStore,
        who: &'static str,
        log: Log,
        /// 締め出しに入った合図。ここまで進めてから先へ動かす。
        entering: Arc<tokio::sync::Notify>,
    }

    type Log = Arc<StdMutex<Vec<String>>>;

    fn log() -> Log {
        Arc::default()
    }

    impl Watched {
        fn open(dir: &std::path::Path, who: &'static str, log: &Log) -> Self {
            Self {
                inner: FileStore::open(dir).unwrap(),
                who,
                log: Arc::clone(log),
                entering: Arc::new(tokio::sync::Notify::new()),
            }
        }

        fn note(&self, what: &str) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} {what}", self.who));
        }

        fn bell(&self) -> Arc<tokio::sync::Notify> {
            Arc::clone(&self.entering)
        }
    }

    /// 手放したことも帳面に残す権利。
    struct Noted {
        who: &'static str,
        log: Log,
        /// 先に落とすと、待っている側が起きてから記録することになる。
        /// [`Drop`] の本体はフィールドより先に走るので順序は保たれる。
        _inner: <FileStore as Persistence>::Guard,
    }

    impl Drop for Noted {
        fn drop(&mut self) {
            self.log.lock().unwrap().push(format!("{} frees", self.who));
        }
    }

    impl Persistence for Watched {
        type Guard = Noted;

        fn load(&self, id: &CredentialId) -> Result<StoredCredential> {
            self.inner.load(id)
        }
        fn store(&self, id: &CredentialId, value: &StoredCredential) -> Result<()> {
            self.note("writes");
            self.inner.store(id, value)
        }
        fn list(&self) -> Result<Vec<CredentialId>> {
            self.inner.list()
        }
        fn lock(&self, id: &CredentialId) -> Result<Self::Guard> {
            // 掴みに行く直前に知らせる。相手が持っている限り、この後は待つ。
            self.note("waits");
            self.entering.notify_one();
            let inner = self.inner.lock(id)?;
            self.note("holds");
            Ok(Noted {
                who: self.who,
                log: Arc::clone(&self.log),
                _inner: inner,
            })
        }
        fn version(&self, id: &CredentialId) -> Option<u64> {
            self.inner.version(id)
        }
    }

    /// 置き場に認証情報を 1 つ置く。
    fn place(dir: &std::path::Path, c: StoredCredential) -> CredentialId {
        let id = CredentialId::new("c");
        FileStore::open(dir).unwrap().store(&id, &c).unwrap();
        id
    }

    /// 2 つのプロセスが同時に期限切れを見つけても、更新は 1 回で済む。
    ///
    /// 束ねているのはプロセスの中だけなので、ここが崩れると 2 本目が
    /// 使用済みの refresh token を送って弾かれる。
    ///
    /// 順番は待ち合わせで作る: 先行が応答待ちに入り、後発が期限切れを読んで
    /// 締め出しに突き当たったところまで進めてから、応答を返す。
    #[tokio::test]
    async fn two_processes_share_one_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeTokenServer::start_gated().await;
        let id = place(dir.path(), cred(&at(-1)));
        let log = log();

        let ahead = Arc::new(process_over(
            Watched::open(dir.path(), "ahead", &log),
            Some(&server),
        ));
        let behind = Arc::new(process_over(
            Watched::open(dir.path(), "behind", &log),
            Some(&server),
        ));
        let waiting = behind.inner.persistence.bell();

        let first = {
            let (store, id) = (Arc::clone(&ahead), id.clone());
            tokio::spawn(async move { store.acquire(&id).await })
        };
        // 更新先に要求が届いた = 先行は権利を持ったまま応答待ち。
        server.wait_for_hit().await;

        let second = {
            let (store, id) = (Arc::clone(&behind), id.clone());
            tokio::spawn(async move { store.acquire(&id).await })
        };
        // 後発は期限切れを読み終え、締め出しに突き当たっている。
        waiting.notified().await;

        server.release();
        let ahead_token = first.await.unwrap().unwrap().bearer();
        let behind_token = second
            .await
            .unwrap()
            .expect("負けた側も失敗しない")
            .bearer();

        assert_eq!(server.hits(), 1, "更新先を叩くのは 1 回だけ");
        assert_eq!(ahead_token, "Bearer at-1");
        assert_eq!(behind_token, ahead_token, "待たされた側も同じ token を得る");
        assert_eq!(
            *log.lock().unwrap(),
            [
                "ahead waits",
                "ahead holds",
                "behind waits",
                "ahead writes",
                "ahead frees",
                "behind holds",
                "behind frees",
            ],
            "後発が掴めるのは先行が書き終えて手放した後"
        );
    }

    /// 更新の最中に別のプロセスが覚えようとした拒否を、更新が消さない。
    ///
    /// 更新は読み直した時点の内容を土台に書き戻すので、締め出しが無いと
    /// その間に書かれた学習ごと巻き戻る。
    ///
    /// 覚える側は締め出しに突き当たって待つので、その書き込みが済むのは
    /// 更新の後。ここで見たいのは「待たされた結果、両方残る」ことなので、
    /// 待ちに入ったのを見届けてから応答を返す。
    #[tokio::test]
    async fn a_refresh_does_not_swallow_a_denial_recorded_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeTokenServer::start_gated().await;
        let id = place(dir.path(), cred(&at(-1)));

        let log = log();
        let refreshing = Arc::new(process_over(
            Watched::open(dir.path(), "refresh", &log),
            Some(&server),
        ));
        let recording = Arc::new(process_over(
            Watched::open(dir.path(), "record", &log),
            None,
        ));
        let waiting = recording.inner.persistence.bell();

        let refresh = {
            let (store, id) = (Arc::clone(&refreshing), id.clone());
            tokio::spawn(async move { store.acquire(&id).await })
        };
        server.wait_for_hit().await;

        let record = {
            let (store, id) = (Arc::clone(&recording), id.clone());
            tokio::spawn(async move {
                store
                    .record_denied_beta(&id, &["advisor-tool-2026-03-01".to_owned()])
                    .await
            })
        };
        waiting.notified().await;

        server.release();
        refresh.await.unwrap().unwrap();
        record.await.unwrap().unwrap();

        let saved = FileStore::open(dir.path()).unwrap().load(&id).unwrap();
        assert_eq!(saved.payload.secret(), "at-1", "更新の結果が残る");
        assert!(
            saved.denied_beta.contains_key("advisor-tool-2026-03-01"),
            "待たされた側の学習も残る"
        );
        assert_eq!(
            *log.lock().unwrap(),
            [
                "refresh waits",
                "refresh holds",
                "record waits",
                "refresh writes",
                "refresh frees",
                "record holds",
                "record writes",
                "record frees",
            ],
            "覚える側は更新が書き終えるまで土台を読まない"
        );
    }

    /// 要求が途中で消えても、更新は最後まで走って次の要求を通す。
    ///
    /// 更新を要求と同じ寿命にすると、クライアントが切っただけで
    /// (1) 進行中の印が外れず、以後その認証情報を求めた全員が結果の来ない
    /// 合図を待ち続け、(2) 送信済みの refresh token が結果を受け取れないまま
    /// 焼ける。どちらも認証情報 1 つを再ログインまで使えなくする。
    #[tokio::test]
    async fn a_cancelled_request_does_not_strand_the_refresh() {
        let server = FakeTokenServer::start_gated().await;
        let store = Arc::new(store_against(Spy::new(cred(&at(-1))), &server).await);
        let id = CredentialId::new("c");

        let cancelled = {
            let (store, id) = (Arc::clone(&store), id.clone());
            tokio::spawn(async move { store.acquire(&id).await })
        };
        // 更新先に要求が届いた = 更新は応答待ち。ここで呼び出し元が消える。
        server.wait_for_hit().await;
        cancelled.abort();
        server.release();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), store.acquire(&id))
            .await
            .expect("次の要求が待ちっぱなしにならない")
            .expect("更新は最後まで走っている");

        assert_eq!(got.bearer(), "Bearer at-1");
        assert_eq!(server.hits(), 1, "同じ refresh token で 2 度叩かない");
        assert!(
            store.inner.in_flight().is_empty(),
            "進行中の印が残っていると次回以降が永久に待つ"
        );
    }

    /// 別のプロセスの書き込みに、期限を待たずに気づく。
    ///
    /// 控えの期限が切れるまでディスクを見ないと、他のプロセスが更新した
    /// 結果も、手で直した内容も、何時間も反映されない。
    #[tokio::test]
    async fn a_write_from_another_process_is_seen_before_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let id = place(dir.path(), cred(&at(3600)));
        let mine = another_process(dir.path(), None);
        let elsewhere = FileStore::open(dir.path()).unwrap();

        assert_eq!(mine.acquire(&id).await.unwrap().bearer(), "Bearer at-1");

        // 期限はまだ先のまま。控えの期限切れでは気づけない書き換え。
        elsewhere
            .store(&id, &cred_with("at-elsewhere", &at(3600)))
            .unwrap();

        assert_eq!(
            mine.acquire(&id).await.unwrap().bearer(),
            "Bearer at-elsewhere"
        );
    }

    #[tokio::test]
    async fn valid_token_is_returned_as_is() {
        let store = store_with(cred(&at(3600)));
        let got = store.acquire(&CredentialId::new("c")).await.unwrap();

        assert_eq!(got.bearer(), "Bearer at-1");
        assert_eq!(
            store.inner.persistence.stores.load(Ordering::SeqCst),
            0,
            "まだ有効なら更新しない"
        );
    }

    /// 期限まで余裕があるかどうかの境目。
    #[test]
    fn refresh_margin() {
        let store = store_with(cred(""));
        assert!(
            !store
                .inner
                .needs_refresh(&cred(&at(REFRESH_MARGIN_SECS + 1)))
        );
        assert!(store.inner.needs_refresh(&cred(&at(REFRESH_MARGIN_SECS))));
        assert!(store.inner.needs_refresh(&cred(&at(0))), "期限ちょうど");
        assert!(store.inner.needs_refresh(&cred(&at(-1))), "切れている");
    }

    /// 期限が壊れていても更新に走らない。refresh token を無駄に使わない。
    #[test]
    fn unreadable_expiry_does_not_trigger_refresh() {
        let store = store_with(cred(""));
        for bad in ["", "not-a-date", "2026-07", "yesterday"] {
            assert!(!store.inner.needs_refresh(&cred(bad)), "{bad:?}");
        }
    }

    /// 更新の口が無い認証情報は、期限が切れていても更新に走らない。
    ///
    /// 走らせても直らないうえ、失敗の理由が「キーの期限切れ」から
    /// 「更新できません」にすり替わって原因が見えなくなる。
    #[test]
    fn api_key_is_never_refreshed() {
        let store = store_with(api_key_cred(&at(-1)));
        assert!(!store.inner.needs_refresh(&api_key_cred(&at(-1))));
        assert!(!store.inner.needs_refresh(&api_key_cred(&at(0))));
    }

    /// それでも更新を頼まれたら、何をすればよいかを言って断る。
    #[tokio::test]
    async fn refreshing_an_api_key_says_what_to_do() {
        let store = store_with(api_key_cred(&at(-1)));
        let err = store
            .inner
            .do_refresh(&CredentialId::new("c"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-OAuth"), "{err}");
        assert!(err.contains("Issue"), "gives the fix: {err}");
    }

    /// API キーはそのまま渡る (期限を理由に握りつぶさない)。
    #[tokio::test]
    async fn api_key_is_handed_out_as_is() {
        let store = store_with(api_key_cred(&at(-1)));
        let got = store.acquire(&CredentialId::new("c")).await.unwrap();
        assert_eq!(got.api_key(), "ak-1");
        assert_eq!(store.inner.persistence.stores.load(Ordering::SeqCst), 0);
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

        let saved = store.inner.persistence.current.lock().unwrap().clone();
        assert_eq!(
            saved.denied_beta.get("advisor-tool-2026-03-01").unwrap(),
            &format_rfc3339(NOW),
            "確認した時刻を一緒に残す"
        );
        assert_eq!(store.inner.persistence.stores.load(Ordering::SeqCst), 1);
    }

    /// 覚えることが無ければ書かない (無駄な書き込みで競合を増やさない)。
    #[tokio::test]
    async fn recording_nothing_does_not_write() {
        let store = store_with(cred(&at(3600)));
        store
            .record_denied_beta(&CredentialId::new("c"), &[])
            .await
            .unwrap();
        assert_eq!(store.inner.persistence.stores.load(Ordering::SeqCst), 0);
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
            store.inner.in_flight().is_empty(),
            "進行中の印が残っていると次回以降が永久に待つ"
        );
    }

    /// 期限切れなら更新し、新しい token を返して保存する。
    #[tokio::test]
    async fn expired_token_is_refreshed_and_persisted() {
        let server = FakeTokenServer::start(std::time::Duration::ZERO).await;
        let store = store_against(Spy::new(cred(&at(-1))), &server).await;
        let id = CredentialId::new("c");

        let got = store.acquire(&id).await.unwrap();

        assert_eq!(got.bearer(), "Bearer at-1", "新しい token が返る");
        assert_eq!(server.hits(), 1);
        assert_eq!(store.inner.persistence.stores.load(Ordering::SeqCst), 1);

        let saved = store.inner.persistence.current.lock().unwrap().clone();
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
        let store = Arc::new(store_against(Spy::new(cred(&at(-1))), &server).await);
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
            store.inner.persistence.stores.load(Ordering::SeqCst),
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
        let store = store_against(Spy::new(cred(&at(-1))), &server).await;
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
        assert!(store.inner.in_flight().is_empty());

        // 2 回目も同じように失敗する (待ちっぱなしにならない)。
        assert!(store.acquire(&id).await.is_err());
    }

    /// 更新が途中で落ちても、待っている側が起きて次の更新に入り直せる。
    ///
    /// 印を外して結果を配るのは更新を走らせた側なので、途中で panic すると
    /// どちらも起きない。印が残ると、その認証情報を求めた全員が来ない合図を
    /// 待ち続け、process を入れ替えるまで戻らない。
    #[tokio::test]
    async fn a_panicking_refresh_does_not_strand_the_next_request() {
        let server = FakeTokenServer::start(std::time::Duration::ZERO).await;
        // 2 回目の読み出し = 更新に入ってからの読み直し。そこで落ちる。
        let store = store_against(Spy::new(cred(&at(-1))).panicking(2), &server).await;
        let id = CredentialId::new("c");

        let failed = tokio::time::timeout(std::time::Duration::from_secs(5), store.acquire(&id))
            .await
            .expect("落ちた更新を待ち続けない")
            .unwrap_err()
            .to_string();

        assert!(failed.contains("unexpectedly"), "{failed}");
        assert_eq!(server.hits(), 0, "更新先へ行く前に落ちている");
        assert!(
            store.inner.in_flight().is_empty(),
            "印が残ると次回以降が永久に待つ"
        );

        // 次の要求は普通に更新できる (道を塞いだままにしない)。
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), store.acquire(&id))
            .await
            .expect("次の要求も待ちっぱなしにならない")
            .expect("更新に入り直せる");

        assert_eq!(got.bearer(), "Bearer at-1");
        assert_eq!(server.hits(), 1);
    }

    /// 別のプロセスが先に更新していたら、更新に走らずその結果を使う。
    ///
    /// 控えだけを見ていると、有効な refresh token を送って焼いてしまう。
    /// refresh token は 1 回しか使えないので、焼いた側も相手も失う。
    #[tokio::test]
    async fn a_newer_disk_is_used_instead_of_refreshing() {
        let server = FakeTokenServer::start(std::time::Duration::ZERO).await;
        // 1 回目の読み出しで控えに古い内容が乗り、2 回目 (更新の直前) で
        // 別のプロセスが書いた内容に入れ替わる。
        let disk = Spy::new(cred(&at(-1))).swapping(2, cred_with("at-elsewhere", &at(3600)));
        let store = store_against(disk, &server).await;

        let got = store.acquire(&CredentialId::new("c")).await.unwrap();

        assert_eq!(got.bearer(), "Bearer at-elsewhere");
        assert_eq!(server.hits(), 0, "更新先を叩かない");
        assert_eq!(store.inner.persistence.stores.load(Ordering::SeqCst), 0);
    }

    /// 断られても、置き場が新しくなっていれば回復する。
    ///
    /// 別のプロセスが一足先に同じ refresh token を使い切った状況。
    /// ここで諦めると、そのプロセスは再ログインするまで戻れない。
    #[tokio::test]
    async fn a_rejected_refresh_recovers_from_a_newer_disk() {
        let server = FakeTokenServer::start_rejecting().await;
        // 3 回目の読み出し = 断られた後の読み直し。そこで結果が見える。
        let disk = Spy::new(cred(&at(-1))).swapping(3, cred_with("at-elsewhere", &at(3600)));
        let store = store_against(disk, &server).await;

        let got = store.acquire(&CredentialId::new("c")).await.unwrap();

        assert_eq!(got.bearer(), "Bearer at-elsewhere");
        assert_eq!(server.hits(), 1, "断られるまでは 1 回試している");
        assert_eq!(
            store.inner.persistence.stores.load(Ordering::SeqCst),
            0,
            "拾っただけなので自分では書かない"
        );
    }

    /// 置き場も古いままなら、断られた理由をそのまま返す。
    ///
    /// ここが成功に化けると、期限切れの token で転送に進んで原因が見えなくなる。
    #[tokio::test]
    async fn a_rejected_refresh_still_fails_when_the_disk_is_unchanged() {
        let server = FakeTokenServer::start_rejecting().await;
        let store = store_against(Spy::new(cred(&at(-1))), &server).await;

        let err = store
            .acquire(&CredentialId::new("c"))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("log in again"), "{err}");
        assert!(store.inner.in_flight().is_empty());
    }

    /// 拒否された beta を覚えるときも、控えではなく置き場から積み直す。
    ///
    /// 控えを土台にすると、別のプロセスが更新した token を古い値で上書きする。
    #[tokio::test]
    async fn recording_denied_beta_does_not_clobber_a_newer_disk() {
        let disk = Spy::new(cred(&at(3600))).swapping(2, cred_with("at-elsewhere", &at(7200)));
        let store = store_sharing(disk);
        let id = CredentialId::new("c");

        // 1 回目の読み出しで控えに古い内容が乗る。
        store.acquire(&id).await.unwrap();
        store
            .record_denied_beta(&id, &["advisor-tool-2026-03-01".to_owned()])
            .await
            .unwrap();

        let saved = store.inner.persistence.current.lock().unwrap().clone();
        assert_eq!(
            saved.payload.secret(),
            "at-elsewhere",
            "別のプロセスの更新を消さない"
        );
        assert!(saved.denied_beta.contains_key("advisor-tool-2026-03-01"));
    }
}
