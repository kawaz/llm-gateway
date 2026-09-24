//! 認証が今生きているかの観測。

use serde::{Deserialize, Serialize};

/// refresh の最終結果。永続 token ではなく、実行中 gateway の観測状態。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    Ok,
    ReloginRequired,
    Degraded,
    /// この組織には OAuth の利用が許可されていない、と上流が答えた
    /// (DR-0009 追補)。
    ///
    /// ログインは生きている。断りの理由は観測できないので [`AuthState::hint`]
    /// へ回し、再ログインでは直らないので `login_path` も付けない。
    OrgNotAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthState {
    pub status: AuthStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 観測から推し量れる原因と、確かめ方。
    ///
    /// [`Self::status`] が観測した事実だけを名乗るのに対し、こちらは**推定**を
    /// 置く場所。断定できない原因 (例: 組織ごと断られている理由) を状態名へ
    /// 混ぜると、当たっていないときに読み手を誤った対処へ送る。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_path: Option<String>,
    /// この状態を観測した時刻 (Unix ミリ秒)。
    pub observed_at: i64,
}
