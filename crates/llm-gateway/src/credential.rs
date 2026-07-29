//! 認証情報の取得と更新。
//!
//! OAuth の access token は 8 時間で切れ、更新のたびに refresh token も
//! 入れ替わる。古い refresh token は使えなくなるので、同じ認証情報に対する
//! 更新が同時に 2 つ走ると後発が弾かれ、再ログインが要る状態に落ちる。
//! そのため取得は [`CredentialStore::acquire`] に集約し、更新を 1 本に束ねる。

use std::fmt;

use crate::Result;

pub mod file;
pub mod oauth;
pub mod store;
pub mod stored;
pub mod time;

pub use store::{Credential, CredentialStore};
pub use stored::{ApiKey, CodexTokens, Kind, OauthTokens, Payload, StoredCredential};

/// 認証情報の識別子。ファイル名の stem をそのまま使う。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CredentialId(String);

impl CredentialId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 認証情報の置き場所。ここだけ差し替えれば保存先を変えられる。
///
/// 当面の実装は平文ファイル ([`file::FileStore`])。cache-warden の
/// 永続化が固まったらそちらへ移す。移行で得られるのは暗号化だけでなく、
/// 「同じ Team ID で署名されたバイナリからしか読めない」というアクセス制御。
pub trait Persistence: Send + Sync + 'static {
    /// 書き換えの権利。持っている間だけ、同じ置き場を使う他のプロセスを
    /// 締め出せる。手放す (drop する) と待っている相手が起きる。
    type Guard: Send;

    fn load(&self, id: &CredentialId) -> Result<StoredCredential>;
    fn store(&self, id: &CredentialId, value: &StoredCredential) -> Result<()>;
    fn list(&self) -> Result<Vec<CredentialId>>;

    /// 書き換えの権利を取る。取れるまで待つ。
    ///
    /// 待ちは呼び出し元をブロックするので、非同期の側から呼ぶときは
    /// ブロックしてよい場所へ逃がすこと。
    fn lock(&self, id: &CredentialId) -> Result<Self::Guard>;

    /// 今の中身の版。
    ///
    /// 読んだときと値が違えば、他の誰かが書いたと分かる。**同じ値なら
    /// 変わっていないとみなす** — 別物になったのに同じ値が返る置き場では、
    /// その入れ替わりを見落とす (`FileStore` は更新時刻のナノ秒。同じ
    /// 時刻が 2 度使われる粒度の置き場では取りこぼす)。
    ///
    /// 版が無い置き場や、中身がまだ無い場合は `None`。`None` 同士も
    /// 「変わっていない」側。
    fn version(&self, id: &CredentialId) -> Option<u64>;
}
