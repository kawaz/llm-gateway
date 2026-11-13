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
    /// ログインは生きている。断りの応答は理由を持たないので、上流に別途
    /// 聞けた状態を [`AuthState::subscription_status`] に、そこから言える
    /// 対処 (聞けなければ原因の推定) を [`AuthState::hint`] に置く。再ログイン
    /// では直らないので `login_path` は付けない。
    OrgNotAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthState {
    pub status: AuthStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 上流に聞けたサブスクリプションの状態 (上流の語のまま。例: `past_due`)。
    ///
    /// [`AuthStatus::OrgNotAllowed`] の理由を確かめに行き、答えが返った時だけ
    /// 入る。値の意味を決め打ちせずそのまま出し、読み解いた対処は
    /// [`Self::hint`] に置く。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_status: Option<String>,
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
