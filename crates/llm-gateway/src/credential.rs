//! 認証情報の取得と更新。
//!
//! OAuth の access token は 8 時間で切れ、更新のたびに refresh token も
//! 入れ替わる。古い refresh token は使えなくなるので、同じ認証情報に対する
//! 更新が同時に 2 つ走ると後発が弾かれ、再ログインが要る状態に落ちる。
//! そのため取得は [`CredentialStore::acquire`] に集約し、更新を 1 本に束ねる。

use crate::Result;

pub mod file;
pub mod oauth;
pub mod refreshing;
pub mod store;
pub mod stored;
pub use gateway_core::credential::time;
pub use gateway_core::credential::{CredentialId, Persistence};

pub use store::{Credential, CredentialStore};
pub use stored::{ApiKey, CodexTokens, Kind, OauthTokens, Payload, StoredCredential};

/// [`StoredCredential`] を置く置き場。境界に書く名前を短くするためだけの別名で、
/// [`Persistence`] を `Value = StoredCredential` で実装すれば自動で満たす。
pub trait CredentialPersistence: Persistence<Value = StoredCredential> {}

impl<P: Persistence<Value = StoredCredential>> CredentialPersistence for P {}

/// login で得た token を、現在の内容を土台にして保存する。
///
/// 書き換えの権利 (`guard`) の下で読み直してから書く (DR-0010)。
pub fn save_login<P: CredentialPersistence>(
    store: &P,
    guard: &P::Guard,
    kind: Kind,
    tokens: &oauth::Tokens,
) -> Result<StoredCredential> {
    let existing = store.reload(guard).ok();
    let credential = tokens.to_stored(kind, existing.as_ref());
    store.store(guard, &credential)?;
    Ok(credential)
}
