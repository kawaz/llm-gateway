//! core 層のエラー。
//!
//! HTTP のステータスへの対応づけは server 層が行う。ここでは「何が起きたか」
//! だけを持ち、`upstream` 系には**どの経路で**起きたかを含める
//! (プロバイダが複数あるので、どれが落ちたか分からないと調べようがない)。

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// リクエストされたモデルに対応する経路が設定に無い。
    #[error("model `{0}` に対応する経路が設定されていません")]
    UnknownModel(String),

    /// 経路はあるが、全て試して届かなかった。
    #[error("model `{model}` の経路が全て失敗しました ({} 件試行)", attempts.len())]
    AllUpstreamsFailed {
        model: String,
        attempts: Vec<UpstreamAttempt>,
    },

    /// upstream に届いたが、エラー応答が返った。本文はそのまま渡す。
    #[error("{provider} が {status} を返しました")]
    UpstreamStatus {
        provider: String,
        status: u16,
        body: String,
    },

    /// upstream に届かなかった (接続失敗 / タイムアウト)。
    #[error("{provider} に接続できません: {source}")]
    UpstreamUnreachable {
        provider: String,
        #[source]
        source: reqwest::Error,
    },

    /// 認証情報が見つからない、または使える状態にできなかった。
    #[error("認証情報 `{id}` を使えません: {reason}")]
    Credential { id: String, reason: String },

    /// token のリフレッシュに失敗した。
    #[error("`{id}` の token 更新に失敗しました: {reason}")]
    Refresh { id: String, reason: String },

    #[error("設定を読めません: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("JSON を解釈できません: {0}")]
    Json(#[from] serde_json::Error),
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
