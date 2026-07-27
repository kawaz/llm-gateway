//! 認証情報の取得と更新。
//!
//! OAuth の access token は 8 時間で切れ、更新のたびに refresh token も
//! 入れ替わる。古い refresh token は使えなくなるので、同じ認証情報に対する
//! 更新が同時に 2 つ走ると後発が弾かれ、再ログインが要る状態に落ちる。
//! そのため取得は [`CredentialStore::acquire`] に集約し、更新を 1 本に束ねる。
//!
//! 保存は [`Persistence`] に委ね、当面は平文ファイル (cpa 互換 JSON)。

use std::fmt;

use crate::Result;

/// 認証情報の識別子。ファイル名の stem をそのまま使う。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CredentialId(pub String);

impl fmt::Display for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// upstream に載せる認証情報。
///
/// 実体の文字列は握ったまま外へ出さない。ヘッダへ載せるのは
/// [`Credential::apply_to`] だけが行う。
pub struct Credential {
    // TODO: 実装時に埋める (task #6)
}

/// 認証情報を使える状態で受け取る窓口。
///
/// 期限が近ければ内部で更新してから返す。同じ id への同時要求は
/// 1 回の更新に束ねられ、全員が同じ結果を受け取る。
pub trait CredentialStore: Send + Sync {
    fn acquire(
        &self,
        id: &CredentialId,
    ) -> impl std::future::Future<Output = Result<Credential>> + Send;
}

/// 認証情報の置き場所。ここだけ差し替えれば保存先を変えられる。
pub trait Persistence: Send + Sync {
    fn load(&self, id: &CredentialId) -> Result<StoredCredential>;
    fn store(&self, id: &CredentialId, value: &StoredCredential) -> Result<()>;
    fn list(&self) -> Result<Vec<CredentialId>>;
}

/// 保存される形。cpa の auth JSON と同じ項目を持たせ、移行期に共存させる。
pub struct StoredCredential {
    // TODO: 実装時に埋める (task #6)
}
