//! 認証情報を使える状態で渡す窓口。
//!
//! 期限が近ければ更新してから返す。同じ認証情報への同時要求は 1 回の更新に
//! 束ね、全員が同じ結果を受け取る。束ねないと、並行リクエストの数だけ更新が
//! 走り、後発が使用済みの refresh token を送って弾かれ、再ログインが要る
//! 状態に落ちる。
//!
//! 置き場は他のプロセスとも共有しているので、束ねるだけでは足りない。書き換え
//! の間は置き場のロックで締め出し、読み出しでは控えの版を照合して相手の書き
//! 込みに気づく (DR-0010)。
//!
//! 「更新が要るか」「どう更新するか」は [`Refresher`] が決める。ここが持つのは
//! 束ね方・締め出し・版の追従・認証の観測だけ。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{RwLock, broadcast};

use super::time::to_unix_ms;
use super::{AuthState, AuthStatus, CredentialId, Persistence};
use crate::error::{Error, RefreshFailureClass, Result};

/// 値の更新のしかた。置き場の値の型ごとに利用側が実装する。
pub trait Refresher: Send + Sync + 'static {
    type Value: Clone + Send + Sync + 'static;

    /// `now_unix` の時点で更新に入るべきか。更新の口が無いものは常に false。
    ///
    /// 期限が読めないものも false にすること。壊れた値を根拠に refresh token を
    /// 使い切るほうが害が大きい。
    fn needs_refresh(&self, value: &Self::Value, now_unix: i64) -> bool;

    /// 更新して、保存すべき次の値を返す。書き換えの権利の下で呼ばれる。
    ///
    /// 新しい値に刻む時刻 (期限の起点、更新した時刻) は、更新先の応答を
    /// 受けた**後**に `clock` から取ること。送る前に取ると、応答が遅いほど
    /// 期限が手前にずれ、保存した直後から「更新が要る」と判定されて
    /// 取り出すたびに更新を繰り返しうる。
    ///
    /// 失敗は [`Error::Refresh`] で返すと、分類 (再認可が要るか) がそのまま
    /// 待っている側と認証の観測に届く。それ以外の失敗は `Degraded` 扱い。
    fn refresh(
        &self,
        id: &CredentialId,
        current: &Self::Value,
        clock: &Clock,
    ) -> impl Future<Output = Result<Self::Value>> + Send;
}

/// 更新の結果を待っている側へ配るための合図。
#[derive(Debug, Clone)]
struct RefreshFailure {
    reason: String,
    class: RefreshFailureClass,
}

type RefreshSignal = broadcast::Sender<std::result::Result<(), RefreshFailure>>;

/// 待っている側へ配る言葉。
///
/// 更新の失敗は理由だけを渡す。丸ごと渡すと、受け取った側がもう一度
/// [`Error::Refresh`] に包んで「更新に失敗しました: 更新に失敗しました: …」に
/// なる。
fn refresh_failure_of(e: &Error) -> RefreshFailure {
    match e {
        Error::Refresh { reason, class, .. } => RefreshFailure {
            reason: reason.clone(),
            class: *class,
        },
        other => RefreshFailure {
            reason: other.to_string(),
            class: RefreshFailureClass::Degraded,
        },
    }
}

/// 控えの 1 件。読んだ時点の版を一緒に持つ。
///
/// 版を持たないと、他のプロセスが書いた結果に期限切れまで気づけない。
struct Held<V> {
    value: Arc<V>,
    version: Option<u64>,
}

pub struct CredentialStore<P, R>
where
    P: Persistence<Value = R::Value>,
    R: Refresher,
{
    inner: Arc<Inner<P, R>>,
}

/// 中身は共有されているので、複製しても同じ控え・同じ進行中の印を見る。
///
/// 要求から切り離した仕事 (裏で様子を聞きに行く等) へ持ち出すために要る。
impl<P, R> Clone for CredentialStore<P, R>
where
    P: Persistence<Value = R::Value>,
    R: Refresher,
{
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
struct Inner<P, R>
where
    P: Persistence<Value = R::Value>,
    R: Refresher,
{
    persistence: P,
    refresher: R,
    /// 進行中の更新。同じ id への 2 人目以降はここに相乗りする。
    ///
    /// 待たない Mutex なのは、印を外すのが [`RefreshHandoff`] の [`Drop`] で、
    /// そこで await できないため。持っている間にするのは印の出し入れだけで、
    /// 待ちを挟まないので待たない錠で足りる。
    in_flight: Mutex<HashMap<CredentialId, RefreshSignal>>,
    /// 読み出しのたびに置き場を開かないための控え。
    held: RwLock<HashMap<CredentialId, Held<R::Value>>>,
    auth: RwLock<HashMap<CredentialId, AuthState>>,
    clock: Clock,
}

/// 現在時刻の取り出し口。試験で固定するために挟んである。
#[derive(Clone)]
pub enum Clock {
    System,
    /// [`Clock::set`] で置いた時刻 (Unix 秒) を返す。複製は同じ時刻を共有する。
    Manual(Arc<AtomicI64>),
}

impl Clock {
    /// この時刻で止まった時計。
    pub fn fixed(now_unix: i64) -> Self {
        Self::Manual(Arc::new(AtomicI64::new(now_unix)))
    }

    /// 止まった時計の針を動かす。[`Clock::System`] では何もしない。
    pub fn set(&self, now_unix: i64) {
        if let Self::Manual(t) = self {
            t.store(now_unix, Ordering::SeqCst);
        }
    }

    pub fn now_unix(&self) -> i64 {
        match self {
            Self::System => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            Self::Manual(t) => t.load(Ordering::SeqCst),
        }
    }
}

impl<P, R> CredentialStore<P, R>
where
    P: Persistence<Value = R::Value>,
    R: Refresher,
{
    pub fn new(persistence: P, refresher: R) -> Self {
        Self::with_clock(persistence, refresher, Clock::System)
    }

    pub fn with_clock(persistence: P, refresher: R, clock: Clock) -> Self {
        Self {
            inner: Arc::new(Inner {
                persistence,
                refresher,
                in_flight: Mutex::new(HashMap::new()),
                held: RwLock::new(HashMap::new()),
                auth: RwLock::new(HashMap::new()),
                clock,
            }),
        }
    }

    /// 使える値を返す。期限が近ければ更新してから返す。
    pub async fn acquire(&self, id: &CredentialId) -> Result<Arc<R::Value>> {
        self.inner.acquire(id).await
    }

    /// 書き換えの権利の下で、置き場の最新を土台に値を書き換える。
    ///
    /// `edit` は読み直した結果 (読めなかった場合はその失敗) を受け取り、
    /// 保存する値を返す。`None` なら書かない。控えではなく置き場から積み直す
    /// ので、別のプロセスが書いた内容を古い土台で上書きしない (DR-0010)。
    pub async fn update<F>(&self, id: &CredentialId, edit: F) -> Result<Option<R::Value>>
    where
        F: FnOnce(Result<R::Value>) -> Result<Option<R::Value>> + Send,
    {
        self.inner.update(id, edit).await
    }

    /// 認証が生きているかの観測を記録する。
    pub async fn record_auth(&self, id: &CredentialId, outcome: &Result<()>) {
        self.inner.record_auth(id, outcome).await
    }

    /// 置き場に入っている今の版 (DR-0010)。控えではなく置き場を見る。
    ///
    /// 中身を読まずに「他のプロセスが書き換えたか」を知るのに使う。
    /// 版を持たない置き場では常に `None` なので、変わっていない扱いになる。
    pub fn version(&self, id: &CredentialId) -> Option<u64> {
        self.inner.persistence.version(id)
    }

    pub async fn auth_state(&self, id: &CredentialId) -> Option<AuthState> {
        self.inner.auth.read().await.get(id).cloned()
    }

    /// この窓口が使う現在時刻 (Unix 秒)。
    pub fn now_unix(&self) -> i64 {
        self.inner.clock.now_unix()
    }

    pub fn persistence(&self) -> &P {
        &self.inner.persistence
    }

    pub fn refresher(&self) -> &R {
        &self.inner.refresher
    }

    /// 進行中の更新の数。
    pub fn refreshes_in_flight(&self) -> usize {
        self.inner.in_flight().len()
    }
}

impl<P, R> Inner<P, R>
where
    P: Persistence<Value = R::Value>,
    R: Refresher,
{
    async fn acquire(self: &Arc<Self>, id: &CredentialId) -> Result<Arc<R::Value>> {
        let current = self.read(id).await?;

        if !self.needs_refresh(&current) {
            return Ok(current);
        }

        self.refresh_once(id).await?;

        self.read(id).await
    }

    async fn update<F>(self: &Arc<Self>, id: &CredentialId, edit: F) -> Result<Option<R::Value>>
    where
        F: FnOnce(Result<R::Value>) -> Result<Option<R::Value>>,
    {
        // 読んで直して書くまでの間、他のプロセスを締め出す。挟まれると、
        // 相手が書いた更新を古い土台で上書きして消す。
        let guard = self.lock(id).await?;
        let current = self.reload_locked(&guard, id).await.map(|v| (*v).clone());
        let Some(next) = edit(current)? else {
            return Ok(None);
        };
        self.persistence.store(&guard, &next)?;
        self.remember(id, next.clone()).await;
        Ok(Some(next))
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
    async fn read(&self, id: &CredentialId) -> Result<Arc<R::Value>> {
        let version = self.persistence.version(id);
        if let Some(hit) = self.held.read().await.get(id) {
            if hit.version == version {
                return Ok(Arc::clone(&hit.value));
            }
            self.auth.write().await.remove(id);
        }
        self.reload(id).await
    }

    /// 置き場から読み直し、控えを入れ替える。
    ///
    /// 同じ置き場を複数のプロセスが共有しているので、控えだけを見ていると
    /// 他のプロセスが書いた結果に気づけない。
    async fn reload(&self, id: &CredentialId) -> Result<Arc<R::Value>> {
        // 版を先に見る。読んだ後に見ると、読み終えてから書かれた中身を
        // 「今の版」として覚え、その更新に気づけなくなる。逆の順なら、
        // 取りこぼしても次の読み出しで版が食い違って読み直しになる。
        let version = self.persistence.version(id);
        let value = self.persistence.load(id)?;
        Ok(self.hold(id, value, version).await)
    }

    /// 書き換えの権利の下で読み直し、控えを入れ替える。
    ///
    /// 書き込みの前と、refresh token を使う前は、ここを通って最新を掴む。
    async fn reload_locked(&self, guard: &P::Guard, id: &CredentialId) -> Result<Arc<R::Value>> {
        let version = self.persistence.version(id);
        let value = self.persistence.reload(guard)?;
        Ok(self.hold(id, value, version).await)
    }

    async fn hold(
        &self,
        id: &CredentialId,
        value: R::Value,
        version: Option<u64>,
    ) -> Arc<R::Value> {
        let value = Arc::new(value);
        self.held.write().await.insert(
            id.clone(),
            Held {
                value: Arc::clone(&value),
                version,
            },
        );
        value
    }

    /// 自分が書いた内容を控えに載せる。
    ///
    /// 版は書いた後に読む。書き換えの権利を持っている間しか呼ばないので、
    /// この間に他のプロセスが割り込むことはない。
    async fn remember(&self, id: &CredentialId, value: R::Value) {
        let version = self.persistence.version(id);
        self.hold(id, value, version).await;
    }

    fn needs_refresh(&self, value: &R::Value) -> bool {
        self.refresher.needs_refresh(value, self.clock.now_unix())
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
                        me.record_auth(&owned, &outcome).await;
                        handoff.finish(outcome.as_ref().map(|_| ()).map_err(refresh_failure_of));
                    });
                    rx
                }
            }
        };

        match result.recv().await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(failure)) => Err(Error::Refresh {
                id: id.to_string(),
                reason: failure.reason,
                class: failure.class,
            }),
            // 結果が配られる前に消えた = 更新できたか分からない。
            Err(_) => Err(Error::Refresh {
                id: id.to_string(),
                reason: "did not receive the refresh result".to_owned(),
                class: RefreshFailureClass::Degraded,
            }),
        }
    }

    async fn record_auth(&self, id: &CredentialId, outcome: &Result<()>) {
        let observed_at_ms = to_unix_ms(self.clock.now_unix());
        let (status, reason) = match outcome {
            Ok(()) => (AuthStatus::Ok, None),
            Err(error) => {
                let failure = refresh_failure_of(error);
                let status = match failure.class {
                    RefreshFailureClass::ReloginRequired => AuthStatus::ReloginRequired,
                    RefreshFailureClass::Degraded => AuthStatus::Degraded,
                };
                (status, Some(failure.reason))
            }
        };
        self.auth.write().await.insert(
            id.clone(),
            AuthState {
                status,
                reason,
                hint: None,
                login_path: None,
                observed_at: observed_at_ms,
            },
        );
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
        let guard = self.lock(id).await?;

        let current = self.reload_locked(&guard, id).await?;

        // 読み直した時点で期限に余裕があるなら、別のプロセスが更新を済ませて
        // いる。ここで走らせても、有効な refresh token を 1 つ捨てるだけ。
        if !self.needs_refresh(&current) {
            return Ok(());
        }

        let next = match self.refresher.refresh(id, &current, &self.clock).await {
            Ok(next) => next,
            // 断られた理由が「別のプロセスが先に使った」なら、その結果はもう
            // 置き場にある。拾えたら回復し、拾えなければ元の理由を返す。
            //
            // Design rationale: 失敗の種別で振り分けていない。読み直して有効
            // なら成功、古いままなら失敗、という判定は理由に依らず正しく、
            // 種別を見分けるには理由の文字列を当てにするしかないため。
            Err(e) => {
                // 読み直せなかったときも断られた理由を返す。ここを `?` にすると
                // 原因が「更新を断られた」から「置き場を読めない」にすり替わる。
                let Ok(latest) = self.reload_locked(&guard, id).await else {
                    return Err(e);
                };
                if self.needs_refresh(&latest) {
                    return Err(e);
                }
                return Ok(());
            }
        };

        // 保存が先。ここで落ちると新しい token を失うが、控えだけ更新して
        // 保存に失敗するよりはよい (次回起動時に古い token で動こうとして
        // 弾かれ、原因が分からなくなる)。
        self.persistence.store(&guard, &next)?;
        self.remember(id, next).await;
        Ok(())
    }
}

/// 切り離した更新の後始末。落ちるときに印を外して結果を配る。
///
/// 走り切った経路だけで後始末をすると、途中で panic した場合に印が残り、
/// その認証情報を求めた全員が来ない合図を待ち続ける (process を入れ替える
/// まで戻らない)。[`Drop`] に寄せておけば、走り切っても落ちても同じ道を通る。
struct RefreshHandoff<P, R>
where
    P: Persistence<Value = R::Value>,
    R: Refresher,
{
    inner: Arc<Inner<P, R>>,
    id: CredentialId,
    tx: RefreshSignal,
    /// 走り切った結果。無いまま落ちたら、途中で途切れたということ。
    outcome: Option<std::result::Result<(), RefreshFailure>>,
}

impl<P, R> RefreshHandoff<P, R>
where
    P: Persistence<Value = R::Value>,
    R: Refresher,
{
    fn new(inner: Arc<Inner<P, R>>, id: CredentialId, tx: RefreshSignal) -> Self {
        Self {
            inner,
            id,
            tx,
            outcome: None,
        }
    }

    /// 走り切った結果を預ける。配るのは落ちるとき。
    fn finish(&mut self, outcome: std::result::Result<(), RefreshFailure>) {
        self.outcome = Some(outcome);
    }
}

impl<P, R> Drop for RefreshHandoff<P, R>
where
    P: Persistence<Value = R::Value>,
    R: Refresher,
{
    fn drop(&mut self) {
        // 印を外してから配る。逆にすると、起きた側が残った印を見て次の
        // 更新に入れなくなる。
        self.inner.in_flight().remove(&self.id);

        // 途切れた理由は追わない。待っている側にできるのは「もう一度頼む」
        // だけなので、panic の中身を運んでも打つ手は変わらない。
        let outcome = self.outcome.take().unwrap_or_else(|| {
            Err(RefreshFailure {
                reason: "the refresh task ended unexpectedly".to_owned(),
                class: RefreshFailureClass::Degraded,
            })
        });
        let _ = self.tx.send(outcome);
    }
}
