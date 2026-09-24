//! 汎用層のエラー。
//!
//! 「何が起きたか」だけを持つ。応答の形への対応づけは利用側が行う。

pub type Result<T> = std::result::Result<T, Error>;

/// 更新の失敗が、人の手を要するかどうか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailureClass {
    /// 認可をやり直すまで直らない。
    ReloginRequired,
    /// 時間を置けば直りうる。
    Degraded,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 認証情報が見つからない、または使える状態にできなかった。
    #[error("could not use credential `{id}`: {reason}")]
    Credential { id: String, reason: String },

    /// token のリフレッシュに失敗した。
    #[error("could not refresh the token for `{id}`: {reason}")]
    Refresh {
        id: String,
        reason: String,
        class: RefreshFailureClass,
    },

    /// 認可 (login) が最後まで進まなかった。
    #[error("could not complete authorization: {reason}")]
    Login { reason: String },

    #[error("could not read the configuration: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("could not parse JSON: {0}")]
    Json(#[from] serde_json::Error),
}
