//! 運用系の口の呼び手の鍵を、呼び手の `jwks_url` から取って持つ (DR-0043 決定 5・6)。
//!
//! キャッシュの鍵は `jwks_url` で、値は最後に取れた JWKS の全体。取り直しに成功したら
//! 丸ごと置き換え (載らなくなった `kid` は落ちる)、失敗したら前の束を保つ。最後の成功
//! から [`STALE_AFTER_SECS`] を過ぎた束は信じない。
//!
//! 取り直す契機は 4 つ:
//!
//! - 起動時に全呼び手分 ([`CallerKeys::sync`] を全部取り直す形で)
//! - 設定の読み直しで増えた・変わった `jwks_url` (消えたものは捨てる)
//! - 定期 ([`PERIOD_SECS`] ごとに全呼び手分、[`run`])
//! - 検証で未知の `kid` を見た時 (`jwks_url` ごとに [`REFETCH_INTERVAL_SECS`] に 1 回まで)
//!
//! token の検証の順序は [`gateway_core::caller::verify`] が持ち、ここは鍵を引く口
//! ([`CallerKeys::lookup`]) だけを渡す。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt as _;
use gateway_core::caller::{Missing, PublicKey, PublicKeys, parse_jwks};
use url::Url;

use crate::config::Caller;
use crate::router::Router;

/// 未知の `kid` で取り直す間隔の下限 (秒、`jwks_url` ごと)。
pub const REFETCH_INTERVAL_SECS: i64 = 30;

/// 定期の取り直しの間隔 (秒)。
pub const PERIOD_SECS: i64 = 10 * 60;

/// 最後の成功から束を信じる長さ (秒)。呼び手の鍵寿命 1 時間の 110%。
pub const STALE_AFTER_SECS: i64 = 66 * 60;

/// 取得の接続と全体の timeout。
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// 取得する応答の本文の上限 (バイト)。
pub const MAX_JWKS_BYTES: usize = 64 * 1024;

/// 定期の仕事が起きて壁時計を見直す間隔の上限 (秒)。
const WAKE_CAP_SECS: i64 = 60;

/// [`FetchJwks::fetch`] が返す future。
pub type Fetching<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;

/// `jwks_url` から JWKS の本文を取る口。試験では差し替える。
pub trait FetchJwks: Send + Sync {
    /// 本文を取る。失敗はログに添える理由。
    fn fetch<'a>(&'a self, url: &'a Url) -> Fetching<'a>;
}

/// この用途の reqwest Client で取る ([`jwks_client`])。
pub struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    pub fn new() -> crate::Result<Self> {
        Ok(Self {
            client: jwks_client()?,
        })
    }
}

impl FetchJwks for HttpFetcher {
    fn fetch<'a>(&'a self, url: &'a Url) -> Fetching<'a> {
        Box::pin(async move {
            require_https(url)?;
            get_bounded(&self.client, url).await
        })
    }
}

/// JWKS の取得だけに使う Client。
///
/// redirect を追わず、接続と全体の timeout を [`FETCH_TIMEOUT`] に置く。転送用の
/// Client は上流の生成を待つために全体の timeout を置かず redirect も追うので、
/// 流用しない (DR-0043 決定 5)。proxy と TLS の根は reqwest の既定 (転送用と同じ)。
pub fn jwks_client() -> crate::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(FETCH_TIMEOUT)
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| crate::Error::Internal(format!("could not build the JWKS client: {e}")))
}

/// https でない `jwks_url` へは出ない。設定の検証 (`check`) が断っているが、取りに出る
/// 側でも確かめる。
fn require_https(url: &Url) -> Result<(), String> {
    if url.scheme() == "https" {
        Ok(())
    } else {
        Err(format!(
            "the JWKS URL must use https, not `{}`",
            url.scheme()
        ))
    }
}

/// 1 回だけ GET し、2xx の本文を [`MAX_JWKS_BYTES`] まで読む。redirect (3xx) も失敗。
async fn get_bounded(client: &reqwest::Client, url: &Url) -> Result<Vec<u8>, String> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("the JWKS URL answered {status}"));
    }
    let too_large = || format!("the JWKS exceeds {MAX_JWKS_BYTES} bytes");
    if response
        .content_length()
        .is_some_and(|length| length > MAX_JWKS_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if body.len() + chunk.len() > MAX_JWKS_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// 設定の呼び手が使う `jwks_url` の集合。
pub fn jwks_urls(callers: &BTreeMap<String, Caller>) -> BTreeSet<Url> {
    callers
        .values()
        .map(|caller| caller.jwks_url.clone())
        .collect()
}

/// `jwks_url` 1 つ分の持ち物。
#[derive(Debug, Default)]
struct Entry {
    keys: PublicKeys,
    /// 最後に取れた取得の、取りに出た時刻 (unix 秒)。一度も取れていなければ `None`。
    fetched_at: Option<i64>,
    /// 最後に未知の `kid` で取りに出た時刻 (unix 秒)。
    refetched_at: Option<i64>,
}

/// 呼び手の鍵のキャッシュ (`jwks_url` → JWKS)。unit のプロセスのメモリに持つ。
///
/// Design rationale: 未知の `kid` の取り直しの間隔は、未知の `kid` で取りに出た回
/// だけで数える (起動時・読み直し・定期の取得は数えない)。間隔は未知の `kid` の
/// 連打で取得を強いられないための歯止めで、起動・読み直し・定期は呼び手が強いられ
/// ない契機なので、これらを数えても歯止めは強まらない。数えると、起動時の取得が
/// 呼び手の起動と重なって失敗した直後の最初の request が取り直せず、「最初の
/// request で取り直す」(DR-0043 決定 5) が間隔の分だけ遅れる。
pub struct CallerKeys {
    fetcher: Arc<dyn FetchJwks>,
    entries: Mutex<HashMap<Url, Entry>>,
}

impl CallerKeys {
    pub fn new(fetcher: Arc<dyn FetchJwks>) -> Self {
        Self {
            fetcher,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// `jwks_url` の束から `kid` の鍵を引く。
    ///
    /// 最後の成功から [`STALE_AFTER_SECS`] を過ぎていれば `kid` を見ずに
    /// [`Missing::Stale`]。一度も取れていない・`kid` が無い時は [`Missing::UnknownKid`]。
    pub fn lookup(&self, url: &Url, kid: &str, now: i64) -> Result<PublicKey, Missing> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(url).ok_or(Missing::UnknownKid)?;
        let fetched_at = entry.fetched_at.ok_or(Missing::UnknownKid)?;
        if now.saturating_sub(fetched_at) > STALE_AFTER_SECS {
            return Err(Missing::Stale);
        }
        entry.keys.get(kid).copied().ok_or(Missing::UnknownKid)
    }

    /// 未知の `kid` を見た時に取り直す。前の取り直しから [`REFETCH_INTERVAL_SECS`] 経って
    /// いなければ取りに出ずに `false`。取りに出たら (成否を問わず) `true`。
    pub async fn refetch_for_unknown_kid(&self, url: &Url, now: i64) -> bool {
        {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let entry = entries.entry(url.clone()).or_default();
            if entry
                .refetched_at
                .is_some_and(|at| now < at.saturating_add(REFETCH_INTERVAL_SECS))
            {
                return false;
            }
            entry.refetched_at = Some(now);
        }
        // 失敗は fetch がログに出し、束は前のまま。
        let _ = self.fetch(url, now).await;
        true
    }

    /// 設定の `jwks_url` の集合 (`urls`) に合わせる。集合に無い `jwks_url` の束を捨て、
    /// 集合の `jwks_url` のうち `all` なら全部、そうでなければまだ持っていないものだけを
    /// 並行して取る。取れなかった `jwks_url` と理由を返す (束は前のまま)。
    pub async fn sync(&self, urls: &BTreeSet<Url>, all: bool, now: i64) -> Vec<(Url, String)> {
        let targets: Vec<Url> = {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.retain(|url, _| urls.contains(url));
            urls.iter()
                .filter(|url| {
                    let known = entries.contains_key(*url);
                    if !known {
                        entries.insert((*url).clone(), Entry::default());
                    }
                    all || !known
                })
                .cloned()
                .collect()
        };
        let results = futures_util::future::join_all(
            targets
                .iter()
                .map(|url| async move { (url, self.fetch(url, now).await) }),
        )
        .await;
        results
            .into_iter()
            .filter_map(|(url, result)| result.err().map(|why| (url.clone(), why)))
            .collect()
    }

    /// 1 回取り、取れたら束を丸ごと置き換える。取れなければログに出して前の束を保つ。
    ///
    /// 持っていない `jwks_url` (取っている間に読み直しで消えたもの) の結果は捨てる。
    /// 遅れて終わった古い取得が、後から取れた束を上書きしないよう、取りに出た時刻が
    /// 今の束より前の結果も捨てる。
    async fn fetch(&self, url: &Url, now: i64) -> Result<(), String> {
        let fetched = self
            .fetcher
            .fetch(url)
            .await
            .and_then(|body| parse_jwks(&body, url.as_str()));
        match fetched {
            Ok(keys) => {
                let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = entries.get_mut(url)
                    && entry.fetched_at.is_none_or(|at| at <= now)
                {
                    tracing::debug!(jwks_url = %url, kids = keys.len(), "fetched the keys of a caller");
                    entry.keys = keys;
                    entry.fetched_at = Some(now);
                }
                Ok(())
            }
            Err(why) => {
                tracing::warn!(jwks_url = %url, %why, "cannot fetch the keys of a caller; keeping the keys from the last fetch");
                Err(why)
            }
        }
    }

    /// 持っている `jwks_url` (試験用)。
    #[cfg(test)]
    pub(crate) fn urls(&self) -> BTreeSet<Url> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

/// 次に起きるまでの長さ。次の時刻まで、[`WAKE_CAP_SECS`] を上限に、最低 1 秒。
///
/// Design rationale: 次の時刻まで 1 回で寝ず上限で区切るのは [`crate::key_rotation::wait`]
/// と同じ理由 (tokio の timer は単調時計で数え、macOS の単調時計はシステムのスリープ中に
/// 止まる)。束の期限 ([`STALE_AFTER_SECS`]) は壁時計で見るので、スリープ明けに定期の
/// 取得が単調時計の残りだけ遅れると、その間は全部の呼び手が断られる。起きても時刻が
/// 来ていなければ何も取りに出ない。
fn wait(next: i64, now: i64) -> Duration {
    let secs = next.saturating_sub(now).clamp(1, WAKE_CAP_SECS);
    Duration::from_secs(secs.unsigned_abs())
}

/// 定期の取り直しを回し続ける。最初の回は `first_at` (unix 秒)。各回で、走っている設定の
/// `[callers]` に無い `jwks_url` を捨て、ある `jwks_url` を全部取り直す (`[callers]` が
/// 空なら何も取りに出ない)。
pub async fn run(router: Arc<Router>, keys: Arc<CallerKeys>, first_at: i64) {
    let mut next = first_at;
    loop {
        let now = crate::credential::time::now_unix();
        if now >= next {
            let urls = jwks_urls(&router.active().config().callers);
            keys.sync(&urls, true, now).await;
            next = now.saturating_add(PERIOD_SECS);
        }
        tokio::time::sleep(wait(next, crate::credential::time::now_unix())).await;
    }
}

#[cfg(test)]
pub(crate) mod tests;
