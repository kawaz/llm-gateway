//! 運用系の口の呼び手を、呼び手が公開する JWKS で確かめる部品 (DR-0043)。
//!
//! 呼び手は request ごとに自分の Ed25519 鍵で `request+jwt` の token に署名し、
//! `Authorization: Bearer` で運ぶ。ここに置くのは HTTP に繋がない 3 つ:
//!
//! - [`parse_jwks`] 呼び手の JWKS から、検証に使ってよい鍵だけを取り込む
//! - [`verify`] token を決まった順序で確かめ、呼び手か落ちた段 ([`Reason`]) を返す
//! - [`JtiSet`] `jti` の使用済み集合 (プロセスのメモリ、上限つき)
//!
//! 鍵の取得と取り直し、呼び手の設定、応答の形は利用側が持つ。compact JWS の割り方と
//! 署名の確かめ方は namespace 認証 ([`crate::ns::jwt`]) と同じ部品を使い、`typ` /
//! `aud` / `iss` / `jti` と時刻の規則はここで別に持つ (ns の token とは互いに通らない)。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ed25519_dalek::VerifyingKey;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::ns::jwt::{Compact, decode_object};

/// この用途の token の `typ` (正規化した形)。
pub const TYP: &str = "request+jwt";

/// token の寿命 (`exp` − `iat`) の上限 (秒)。
pub const MAX_LIFETIME_SECS: i64 = 10;

/// `jti` の使用済み集合に持つ件数の上限。
pub const JTI_CAPACITY: usize = 65_536;

/// 断る時の応答の種類。呼び手が署名し直せば通りうるかで分ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 401。署名し直せば通りうる。
    Unauthorized,
    /// 400。直しても通らない (設定か実装の不一致)。
    BadRequest,
}

/// 通さなかった段。ログのためだけにあり、応答では [`Status`] ごとの固定文にする
/// (どの段で落ちたかを外から探らせない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `Authorization` が無い。
    MissingAuthorization,
    /// `Authorization` が `Bearer <token>` の形でない。
    NotBearer,
    /// token が compact JWS として読めない (3 段、base64url、JSON のオブジェクト)。
    Malformed,
    /// header の `alg` が `Ed25519` / `EdDSA` でない。
    AlgNotAccepted,
    /// header の `typ` が `request+jwt` でない。
    TypMismatch,
    /// header に `kid` が無い (空を含む)。
    MissingKid,
    /// 理解できない拡張 (`crit`) を要求している。
    Crit,
    /// claims の `iss` が無い、または呼び手に無い。
    UnknownIss,
    /// 呼び手の鍵に `kid` が無い。
    UnknownKid,
    /// 呼び手の鍵の取得が最後の成功から上限を過ぎていて、鍵を信じられない。
    KeysStale,
    /// 鍵 (JWK) の `alg` が header の `alg` と違う。
    KeyAlgMismatch,
    /// 署名が合わない。
    BadSignature,
    /// `exp` か `iat` が無い。
    MissingTime,
    /// `exp` か `iat` が 0 以上の整数 (i64) でない。
    InvalidTime,
    /// `exp` − `iat` が 0 以上 [`MAX_LIFETIME_SECS`] 以下でない。
    Lifetime,
    /// `exp` を過ぎている (`now` ≥ `exp`)。
    Expired,
    /// `aud` が「method + 空白 + path」と一致しない。
    AudMismatch,
    /// `iss` が引いた呼び手の `iss` と一致しない。
    IssMismatch,
    /// `jti` が文字列でない。
    InvalidJti,
    /// `jti` が使用済み。
    JtiReused,
}

impl Reason {
    /// 応答の種類。
    pub fn status(self) -> Status {
        match self {
            Self::MissingAuthorization
            | Self::UnknownKid
            | Self::KeysStale
            | Self::BadSignature
            | Self::Expired
            | Self::JtiReused => Status::Unauthorized,
            Self::NotBearer
            | Self::Malformed
            | Self::AlgNotAccepted
            | Self::TypMismatch
            | Self::MissingKid
            | Self::Crit
            | Self::UnknownIss
            | Self::KeyAlgMismatch
            | Self::MissingTime
            | Self::InvalidTime
            | Self::Lifetime
            | Self::AudMismatch
            | Self::IssMismatch
            | Self::InvalidJti => Status::BadRequest,
        }
    }

    /// ログに出す語。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingAuthorization => "missing_authorization",
            Self::NotBearer => "not_bearer",
            Self::Malformed => "malformed",
            Self::AlgNotAccepted => "alg_not_accepted",
            Self::TypMismatch => "typ_mismatch",
            Self::MissingKid => "missing_kid",
            Self::Crit => "crit",
            Self::UnknownIss => "unknown_iss",
            Self::UnknownKid => "unknown_kid",
            Self::KeysStale => "keys_stale",
            Self::KeyAlgMismatch => "key_alg_mismatch",
            Self::BadSignature => "bad_signature",
            Self::MissingTime => "missing_time",
            Self::InvalidTime => "invalid_time",
            Self::Lifetime => "lifetime",
            Self::Expired => "expired",
            Self::AudMismatch => "aud_mismatch",
            Self::IssMismatch => "iss_mismatch",
            Self::InvalidJti => "invalid_jti",
            Self::JtiReused => "jti_reused",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 受ける `alg`。どちらも Ed25519 で、曲線は鍵の取り込み ([`parse_jwks`]) で
/// `crv` を確かめて閉じている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    /// fully-specified の `Ed25519` (RFC 9864)。
    Ed25519,
    /// `EdDSA`。
    EdDsa,
}

impl Alg {
    /// JOSE の名前から。大文字小文字は区別する (RFC 7515 §4.1.1)。
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Ed25519" => Some(Self::Ed25519),
            "EdDSA" => Some(Self::EdDsa),
            _ => None,
        }
    }
}

/// 呼び手の公開鍵 1 本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicKey {
    pub key: VerifyingKey,
    /// JWK に `alg` があればその値。あれば header の `alg` と一致を見る。
    pub alg: Option<Alg>,
}

/// 呼び手 1 つの鍵 (`kid` → 公開鍵)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublicKeys(BTreeMap<String, PublicKey>);

impl PublicKeys {
    pub fn get(&self, kid: &str) -> Option<&PublicKey> {
        self.0.get(kid)
    }

    pub fn kids(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// JWKS (`{"keys":[...]}`) から、検証に使ってよい鍵だけを取り込む。
///
/// 取り込むのは `kid` があり、`kty: OKP` / `crv: Ed25519`、`use` があれば `sig`、
/// `alg` があれば `Ed25519` か `EdDSA` の鍵。`d` (秘密の部分) があっても読まず、
/// 公開の部分 (`x`) だけを使う。合わない鍵は飛ばしてログに残し、全体は失敗に
/// しない (1 本の書き間違いで呼び手の鍵を全部失わない)。同じ `kid` が 2 本以上
/// あれば、どちらの鍵か決められないのでその `kid` は全部取り込まない。
///
/// 全体を失敗にするのは、JSON でない、`keys` の配列が無い時だけ。`source` は
/// ログに添える取得元 (URL 等)。
pub fn parse_jwks(body: &[u8], source: &str) -> Result<PublicKeys, String> {
    let jwks: Value =
        serde_json::from_slice(body).map_err(|e| format!("the JWKS is not JSON: {e}"))?;
    let keys = jwks
        .get("keys")
        .and_then(Value::as_array)
        .ok_or("the JWKS has no `keys` array")?;
    let mut taken = BTreeMap::new();
    let mut doubled = BTreeSet::new();
    for (index, jwk) in keys.iter().enumerate() {
        match read_public_jwk(jwk) {
            Ok((kid, key)) => {
                if taken.insert(kid.clone(), key).is_some() {
                    doubled.insert(kid);
                }
            }
            Err(why) => {
                let kid = jwk.get("kid").and_then(Value::as_str).unwrap_or("");
                tracing::warn!(
                    source,
                    index,
                    kid,
                    why,
                    "skipping a JWK that cannot verify request tokens"
                );
            }
        }
    }
    for kid in doubled {
        taken.remove(&kid);
        tracing::warn!(
            source,
            kid,
            "skipping a kid that appears more than once in the JWKS"
        );
    }
    Ok(PublicKeys(taken))
}

/// JWK 1 本を読む。断る理由はログに出す語。
fn read_public_jwk(jwk: &Value) -> Result<(String, PublicKey), &'static str> {
    if !jwk.is_object() {
        return Err("the JWK is not an object");
    }
    let field = |name: &str| jwk.get(name).map(Value::as_str);
    let kid = match field("kid") {
        Some(Some(kid)) if !kid.is_empty() => kid,
        _ => return Err("the JWK has no `kid`"),
    };
    if field("kty") != Some(Some("OKP")) {
        return Err("`kty` is not OKP");
    }
    if field("crv") != Some(Some("Ed25519")) {
        return Err("`crv` is not Ed25519");
    }
    if let Some(key_use) = field("use")
        && key_use != Some("sig")
    {
        return Err("`use` is not sig");
    }
    let alg = match field("alg") {
        None => None,
        Some(name) => Some(
            name.and_then(Alg::from_name)
                .ok_or("`alg` is neither Ed25519 nor EdDSA")?,
        ),
    };
    let x: [u8; 32] = field("x")
        .flatten()
        .and_then(|x| B64.decode(x).ok())
        .and_then(|x| x.try_into().ok())
        .ok_or("`x` is not 32 bytes of base64url")?;
    let key = VerifyingKey::from_bytes(&x).map_err(|_| "`x` is not an Ed25519 public key")?;
    Ok((kid.to_owned(), PublicKey { key, alg }))
}

/// 期待する `aud` (「method + 空白 + path」)。method は受けたまま (大文字小文字を
/// 変えない)、path に query は含めない。
pub fn audience(method: &str, path: &str) -> String {
    format!("{method} {path}")
}

/// 鍵を引けなかった理由 ([`verify`] の段 3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// `iss` に当たる呼び手が無い。
    UnknownIss,
    /// 呼び手はあるが、その鍵に `kid` が無い。
    UnknownKid,
    /// 呼び手の鍵を信じられる期限を過ぎている。
    Stale,
}

/// 引けた呼び手と鍵。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// 呼び手の名前 (設定の表の名前)。
    pub caller: String,
    /// 呼び手の `iss` (実効値)。
    pub iss: String,
    pub key: PublicKey,
}

/// 検証を通った呼び手。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// 呼び手の名前 (設定の表の名前)。
    pub caller: String,
    pub kid: String,
}

/// `Authorization` の値 (`authorization`) の token を確かめる。
///
/// - `audience`: 期待する `aud` ([`audience`] で組む)
/// - `now`: unix 秒
/// - `lookup`: claims の `iss` (署名前なのでまだ信じない) と header の `kid` から、
///   **その呼び手の鍵だけ**を引く。別の呼び手の鍵で試す救済はしない
/// - `jtis`: `jti` の使用済み集合
///
/// 順序は DR-0043 決定 4 の 1〜7 で、1 つ落ちたらそこで終わる。claims の値は
/// 署名が通るまで `iss` で鍵を引く以外に使わない。
///
/// 時刻は `now` < `exp` と 0 ≤ `exp` − `iat` ≤ [`MAX_LIFETIME_SECS`] だけを見て、
/// `now` に猶予を足し引きしない。`iat` ≤ `now` は見ず (`iat` > `now` はログに残して
/// 通す)、`nbf` も見ない。header の `jku` / `jwk` / `x5u` / `x5c` は読まない。
pub fn verify(
    authorization: Option<&[u8]>,
    audience: &str,
    now: i64,
    lookup: impl FnOnce(&str, &str) -> Result<Resolved, Missing>,
    jtis: &JtiSet,
) -> Result<Verified, Reason> {
    // 1. Bearer と compact JWS の形。
    let token = bearer(authorization.ok_or(Reason::MissingAuthorization)?)?;
    let compact = Compact::split(token).ok_or(Reason::Malformed)?;
    let header = decode_object(compact.header).ok_or(Reason::Malformed)?;
    let claims = decode_object(compact.payload).ok_or(Reason::Malformed)?;
    let signature = compact.signature().ok_or(Reason::Malformed)?;

    // 2. header。
    let alg = text(&header, "alg")
        .and_then(Alg::from_name)
        .ok_or(Reason::AlgNotAccepted)?;
    if !text(&header, "typ").is_some_and(is_request_typ) {
        return Err(Reason::TypMismatch);
    }
    let kid = text(&header, "kid")
        .filter(|kid| !kid.is_empty())
        .ok_or(Reason::MissingKid)?;
    if header.get("crit").is_some() {
        return Err(Reason::Crit);
    }

    // 3. 名乗った呼び手の鍵空間だけから kid を引く。
    let claimed_iss = text(&claims, "iss").ok_or(Reason::UnknownIss)?;
    let resolved = lookup(claimed_iss, kid).map_err(|missing| match missing {
        Missing::UnknownIss => Reason::UnknownIss,
        Missing::UnknownKid => Reason::UnknownKid,
        Missing::Stale => Reason::KeysStale,
    })?;
    if resolved.key.alg.is_some_and(|key_alg| key_alg != alg) {
        return Err(Reason::KeyAlgMismatch);
    }

    // 4. 署名。
    if !compact.is_signed_by(&resolved.key.key, &signature) {
        return Err(Reason::BadSignature);
    }

    // 5. 時刻。寿命の上限を先に見る (上限を超える token は署名し直しても通らない)。
    let exp = numeric_date(&claims, "exp")?;
    let iat = numeric_date(&claims, "iat")?;
    let lifetime = exp.checked_sub(iat).ok_or(Reason::InvalidTime)?;
    if !(0..=MAX_LIFETIME_SECS).contains(&lifetime) {
        return Err(Reason::Lifetime);
    }
    if now >= exp {
        return Err(Reason::Expired);
    }
    if iat > now {
        tracing::warn!(caller = %resolved.caller, kid, iat, now, "the caller's clock is ahead of this host; accepting the token");
    }

    // 6. 宛先と名乗り。
    if text(&claims, "aud") != Some(audience) {
        return Err(Reason::AudMismatch);
    }
    if text(&claims, "iss") != Some(resolved.iss.as_str()) {
        return Err(Reason::IssMismatch);
    }

    // 7. jti は付いてきた時だけ見る。
    match claims.get("jti") {
        None => {}
        Some(Value::String(jti)) => {
            if jtis.record(&resolved.caller, jti, exp, now) == JtiOutcome::Reused {
                return Err(Reason::JtiReused);
            }
        }
        Some(_) => return Err(Reason::InvalidJti),
    }

    Ok(Verified {
        caller: resolved.caller,
        kid: kid.to_owned(),
    })
}

/// オブジェクトの文字列の欄。無い・文字列でなければ `None`。
fn text<'v>(object: &'v Value, name: &str) -> Option<&'v str> {
    object.get(name).and_then(Value::as_str)
}

/// `Bearer <token>` から token を取り出す。scheme の大文字小文字は問わない (RFC 9110 §11.1)。
fn bearer(value: &[u8]) -> Result<&str, Reason> {
    let value = std::str::from_utf8(value).map_err(|_| Reason::NotBearer)?;
    let (scheme, token) = value.trim().split_once(' ').ok_or(Reason::NotBearer)?;
    let token = token.trim_start_matches(' ');
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(Reason::NotBearer);
    }
    Ok(token)
}

/// `typ` を正規化 (小文字化、`application/` 接頭辞を落とす、RFC 7515 §4.1.9) して比べる。
fn is_request_typ(typ: &str) -> bool {
    let typ = typ.to_ascii_lowercase();
    typ.strip_prefix("application/").unwrap_or(&typ) == TYP
}

/// 時刻の claim。0 以上の整数 (i64 に収まる) だけを受ける。
fn numeric_date(claims: &Value, name: &str) -> Result<i64, Reason> {
    claims
        .get(name)
        .ok_or(Reason::MissingTime)?
        .as_i64()
        .filter(|t| *t >= 0)
        .ok_or(Reason::InvalidTime)
}

/// [`JtiSet::record`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JtiOutcome {
    /// 初見で、記録した。
    Recorded,
    /// 使用済み。
    Reused,
    /// 集合が満杯で、見ずに通した (記録もしない)。
    Skipped,
}

/// `jti` の使用済み集合。プロセスのメモリに持ち、プロセスの間では共有しない。
///
/// 記録は `exp` まで保持し、`exp` を過ぎたものは次の記録のついでに掃く (`now` ≥
/// `exp` の token は検証の段 5 で落ちるので、保持の寿命に猶予を足さない)。
/// 上限 ([`JTI_CAPACITY`]) に達している間は、来た token の `jti` を見ずに通す。
///
/// Design rationale: 満杯で拒否せず飛ばすのは DR-0043 決定 4 の判断 (読み出しの口で
/// `exp` が短く、飛ばしても実害が無い。満杯で全拒否すると呼び手が全部読めなくなる)。
/// 記録は呼び手の名前と `jti` の SHA-256 で持つ。`jti` の長さは呼び手しだいなので、
/// 上限の件数で使うメモリを `jti` の長さから切り離す。呼び手の名前を混ぜるのは、
/// ある呼び手の `jti` が別の呼び手の `jti` を使用済みにしないため。
#[derive(Debug)]
pub struct JtiSet {
    capacity: usize,
    state: Mutex<JtiState>,
}

#[derive(Debug, Default)]
struct JtiState {
    seen: HashSet<[u8; 32]>,
    /// (`exp`, 記録) の順。掃く時に先頭から取る。
    by_exp: BTreeSet<(i64, [u8; 32])>,
    /// 満杯をログに出した。満杯が解けるまで繰り返さない。
    warned_full: bool,
}

impl Default for JtiSet {
    fn default() -> Self {
        Self::new()
    }
}

impl JtiSet {
    /// 上限 [`JTI_CAPACITY`] の空の集合。
    pub fn new() -> Self {
        Self::with_capacity(JTI_CAPACITY)
    }

    /// 上限を指定した空の集合 (試験用に小さくする)。
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(JtiState::default()),
        }
    }

    /// 呼び手 `caller` の `jti` を、`exp` まで使用済みとして記録する。
    pub fn record(&self, caller: &str, jti: &str, exp: i64, now: i64) -> JtiOutcome {
        let id: [u8; 32] = Sha256::new()
            .chain_update(caller.as_bytes())
            .chain_update([0])
            .chain_update(jti.as_bytes())
            .finalize()
            .into();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.sweep(now);
        if state.seen.len() >= self.capacity {
            if !state.warned_full {
                state.warned_full = true;
                tracing::warn!(
                    capacity = self.capacity,
                    "the set of used jti is full; accepting tokens without checking jti until entries expire"
                );
            }
            return JtiOutcome::Skipped;
        }
        state.warned_full = false;
        if !state.seen.insert(id) {
            return JtiOutcome::Reused;
        }
        state.by_exp.insert((exp, id));
        JtiOutcome::Recorded
    }

    /// 持っている記録の数 (掃いていない期限切れを含む)。
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl JtiState {
    /// `exp` ≤ `now` の記録を捨てる。
    fn sweep(&mut self, now: i64) {
        while let Some(&(exp, id)) = self.by_exp.first() {
            if exp > now {
                break;
            }
            self.by_exp.pop_first();
            self.seen.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests;
