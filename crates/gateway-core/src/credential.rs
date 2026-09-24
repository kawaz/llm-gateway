//! 認証情報の汎用部分。

use std::fmt;

use crate::Result;

pub mod file;
pub mod time;

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

/// 単一 writer の更新の置き場 (DR-0031 §2 (1))。ここだけ差し替えれば保存先を変えられる。
///
/// 書き換えは「[`lock`](Self::lock) で権利を取る → その権利の下で
/// [`reload`](Self::reload) で最新を読み直す → [`store`](Self::store) で書く →
/// 権利を手放す」の 1 単位で行う。読み直しを権利の前にすると、待っている間に
/// 相手が書いたものを古い土台で消す (DR-0010)。書き込みと読み直しは権利
/// ([`Guard`](Self::Guard)) を引数に取るので、権利なしでは書けない。
///
/// 失敗の扱い: `lock` / `reload` / `store` / `load` は fail-closed (失敗を
/// 返し、推測した値で進めない)。`version` は取れなければ版なしと同じ扱い。
///
/// Design rationale: 書き込みを権利のメソッドにせず、権利を引数に取る形に
/// している。権利を取るまでの待ちはブロックするので、利用側は専用の
/// スレッドへ逃がし、権利をそこから持ち帰る。そのため権利は置き場を借用
/// できず (`'static`)、権利のメソッドにすると各実装の権利が置き場への
/// 共有の参照を抱える必要が出る。引数に取れば、権利は締め出しそのもの
/// だけを持てばよい。
pub trait Persistence: Send + Sync + 'static {
    /// 置く値。
    type Value;

    /// 書き換えの権利。持っている間だけ、同じ置き場を使う他のプロセスを
    /// 締め出せる。手放す (drop する) と待っている相手が起きる。どの
    /// 識別子の権利かは権利自身が覚えている。
    type Guard: Send + Sync + 'static;

    /// 書き換えの権利を取る。取れるまで待つ。
    ///
    /// 待ちは呼び出し元をブロックするので、非同期の側から呼ぶときは
    /// ブロックしてよい場所へ逃がすこと。
    fn lock(&self, id: &CredentialId) -> Result<Self::Guard>;

    /// 権利なしの読み出し。
    fn load(&self, id: &CredentialId) -> Result<Self::Value>;

    /// 権利の下で最新を読み直す。書き換えの土台にはこれを使う。
    fn reload(&self, guard: &Self::Guard) -> Result<Self::Value>;

    /// 権利の下で書く。
    fn store(&self, guard: &Self::Guard, value: &Self::Value) -> Result<()>;

    /// 今の中身の版。
    ///
    /// 読んだときと値が違えば、他の誰かが書いたと分かる。**同じ値なら
    /// 変わっていないとみなす** — 別物になったのに同じ値が返る置き場では、
    /// その入れ替わりを見落とす (`FileStore` は更新時刻のナノ秒。同じ
    /// 時刻が 2 度使われる粒度の置き場では取りこぼす)。
    ///
    /// 版が無い置き場や、中身がまだ無い場合、版を取れなかった場合は
    /// `None`。`None` 同士も「変わっていない」側 (DR-0010)。
    fn version(&self, id: &CredentialId) -> Option<u64>;

    fn list(&self) -> Result<Vec<CredentialId>>;
}
