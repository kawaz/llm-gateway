//! core 層のエラー。
//!
//! HTTP のステータスへの対応づけは server 層が行う。ここでは「何が起きたか」
//! だけを持ち、`upstream` 系には**どの経路で**起きたかを含める
//! (プロバイダが複数あるので、どれが落ちたか分からないと調べようがない)。

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

pub use gateway_core::error::RefreshFailureClass;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// リクエストされたモデルに対応する経路が設定に無い。
    #[error("no route configured for model `{0}`")]
    UnknownModel(String),

    /// モデルの経路はあるが、受けた形のまま運べる経路が 1 つも無い (DR-0025)。
    ///
    /// 「モデルが無い」とは別物。同じモデルでも、Messages 形式の受け口からなら
    /// 通ることがある。
    #[error("no route for model `{model}` can carry a `{shape}` request")]
    UnsupportedRequestShape { model: String, shape: &'static str },

    /// 経路の形式へ本文を変換できない (本文側の問題)。
    ///
    /// 同じ本文なら他の経路でも結果は変わらないので、経路は切り替えない。
    #[error("could not translate the request for this route: {0}")]
    UntranslatableRequest(String),

    /// 経路はあるが、全て試して届かなかった。
    #[error("all routes for model `{model}` failed ({} attempts)", attempts.len())]
    AllUpstreamsFailed {
        model: String,
        attempts: Vec<UpstreamAttempt>,
    },

    /// upstream に届いたが、エラー応答が返った。本文はそのまま渡す。
    #[error("{provider} returned {status}")]
    UpstreamStatus {
        provider: String,
        status: u16,
        body: String,
    },

    /// upstream は応えたが、中身が使えない (形が想定外、大きすぎる、読み取りが
    /// 途中で切れた)。`provider` は経路の名前か、経路の名前が手元に無い場所では
    /// 相手の API の名前 (`Messages API` 等)。
    #[error("{provider} returned an unusable response: {reason}")]
    UpstreamResponse { provider: String, reason: String },

    /// gateway 自身の内部の失敗 (設定でも request でも上流でもないもの)。
    #[error("internal error: {0}")]
    Internal(String),

    /// upstream に届かなかった (接続失敗 / タイムアウト)。
    #[error("could not reach {provider}: {source}")]
    UpstreamUnreachable {
        provider: String,
        #[source]
        source: reqwest::Error,
    },

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
    ///
    /// 転送の最中には起きない (CLI からの login でだけ出る)。
    #[error("could not complete authorization: {reason}")]
    Login { reason: String },

    #[error("could not read the configuration: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("could not parse JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// 汎用層のエラーを同じ名前の variant へ写す。
///
/// 包む variant を足さないのは、利用側 (server) が `Refresh` 等を直に
/// 照合しているため。包むと照合が 1 段深くなる (gateway-core-split §6.3)。
impl Error {
    /// [`Error::UpstreamResponse`] を作る。
    pub fn upstream_response(provider: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::UpstreamResponse {
            provider: provider.into(),
            reason: reason.into(),
        }
    }
}

impl From<gateway_core::Error> for Error {
    fn from(e: gateway_core::Error) -> Self {
        use gateway_core::Error as Core;
        match e {
            Core::Credential { id, reason } => Self::Credential { id, reason },
            Core::Refresh { id, reason, class } => Self::Refresh { id, reason, class },
            Core::Login { reason } => Self::Login { reason },
            Core::Config(reason) => Self::Config(reason),
            Core::Io(e) => Self::Io(e),
            Core::Json(e) => Self::Json(e),
        }
    }
}

/// 1 経路ぶんの失敗記録。どこで何が起きたかを残す。
#[derive(Debug, Clone)]
pub struct UpstreamAttempt {
    pub provider: String,
    pub reason: String,
}

impl fmt::Display for UpstreamAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.provider, self.reason)
    }
}
