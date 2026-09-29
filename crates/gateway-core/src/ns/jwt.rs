//! namespace 認証の `jwt` 方式 (DR-0030 §6)。
//!
//! JWS compact (`b64(header).b64(payload).b64(sig)`) を自前で読み、Ed25519 で
//! 検証する。**検証の方式は鍵束の行の `kty` / `crv` で決まり、ヘッダの `alg` は
//! 照合にしか使わない** — 鍵の型 ([`VerifyingKey`]) が alg そのものなので、ヘッダの
//! 申告で検証の方法が変わる経路が構造上無い。
//!
//! 失敗の理由 ([`Reason`]) は呼び出し側のログのためだけに返す。応答で区別すると、
//! kid の有無や期限切れを外から探らせることになる。

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::UNIX_EPOCH;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
pub use ed25519_dalek::{SigningKey, VerifyingKey};

use ed25519_dalek::Signature;
use serde_json::Value;

/// 時計の揺れの許し (秒)。
pub const SKEW_SECS: i64 = 60;

/// `sub` の長さの上限 (バイト)。知らせのキーになるので長さを抑える。
const MAX_SUBJECT_LEN: usize = 128;

/// 通さなかった理由。応答には出さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 3 段に割れない、base64url / JSON として読めない、必須の claim が無い等。
    Malformed,
    /// ヘッダの `kid` が無い、または設定に無い。
    UnknownKid,
    /// ヘッダの `alg` が kid の設定値と違う (`none` を含む)。
    AlgMismatch,
    /// 理解できない拡張 (`crit`) を要求している。
    Crit,
    /// 署名が合わない。
    BadSignature,
    /// `exp` を過ぎている。
    Expired,
    /// `nbf` より前、または `iat` が先の時刻。
    NotYetValid,
    /// 寿命が `max_ttl` を超える。
    TtlExceeded,
    /// `sub` / `iss` / `aud` が要件に合わない。
    Claims,
}

impl Reason {
    /// ログに出す語。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnknownKid => "unknown_kid",
            Self::AlgMismatch => "alg_mismatch",
            Self::Crit => "crit",
            Self::BadSignature => "bad_signature",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::TtlExceeded => "ttl_exceeded",
            Self::Claims => "claims",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 検査を通った token の主体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub subject: String,
    pub kid: String,
}

/// 鍵束の各行から秘密鍵を読み、kid で引ける形にする。エラーには秘密鍵の内容を含めない。
pub fn read_key_ring(text: &str) -> Result<BTreeMap<String, SigningKey>, String> {
    let mut keys = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_no = index + 1;
        let (kid, key) = read_private_jwk(line).map_err(|e| format!("line {line_no}: {e}"))?;
        let kid = kid.ok_or_else(|| format!("line {line_no}: JWK needs `kid`"))?;
        if keys.insert(kid.clone(), key).is_some() {
            return Err(format!(
                "line {line_no}: duplicate kid `{kid}`; remove one line"
            ));
        }
    }
    if keys.is_empty() {
        return Err(
            "the key ring has no keys; append one with `llm-gateway auth keygen >> <file>`"
                .to_owned(),
        );
    }
    Ok(keys)
}

/// kid → 公開鍵の表。読み手は `Arc` を clone してロックの外で使う。
type Table = Arc<BTreeMap<String, VerifyingKey>>;

/// 検証用の鍵束 (kid → 公開鍵)。
///
/// ファイルから作ったものは、[`KeyRing::current`] のたびに mtime を見て、変わって
/// いれば読み直す。読み直しに失敗したら前の束を保つ (書きかけのファイルで全鍵を
/// 失わない)。1 行でも不正なら束全体を採らない。
///
/// clone は中身を共有する。設定を clone して持つ複数の部品が、それぞれ別に読み直さない。
#[derive(Clone)]
pub struct KeyRing(Arc<Inner>);

struct Inner {
    /// 鍵束の置き場。[`KeyRing::from_keys`] で作ったものは `None` (読み直さない)。
    path: Option<PathBuf>,
    /// 最後に見た mtime (読み直しに失敗した時も記録する) と、最後に読めた kid → 公開鍵。
    state: RwLock<(Option<u64>, Table)>,
}

impl KeyRing {
    /// 鍵束ファイルを読む。読めなければ (無い・権限・不正行) エラー。
    pub fn from_file(path: PathBuf) -> Result<Self, String> {
        let (mtime, keys) = load(&path)?;
        Ok(Self(Arc::new(Inner {
            path: Some(path),
            state: RwLock::new((Some(mtime), Arc::new(keys))),
        })))
    }

    /// 与えた表をそのまま持つ (読み直さない)。
    pub fn from_keys(keys: BTreeMap<String, VerifyingKey>) -> Self {
        Self(Arc::new(Inner {
            path: None,
            state: RwLock::new((None, Arc::new(keys))),
        }))
    }

    /// 鍵束ファイルの置き場。
    pub fn path(&self) -> Option<&Path> {
        self.0.path.as_deref()
    }

    /// 最後に読んだ束の kid (読み直しはしない)。
    pub fn kids(&self) -> Vec<String> {
        self.snapshot().keys().cloned().collect()
    }

    fn snapshot(&self) -> Table {
        Arc::clone(&self.0.state.read().unwrap_or_else(|e| e.into_inner()).1)
    }

    /// 今の束。ファイルの mtime が記録と違えば読み直してから返す。
    pub fn current(&self) -> Arc<BTreeMap<String, VerifyingKey>> {
        let Some(path) = &self.0.path else {
            return self.snapshot();
        };
        let mtime = match mtime_of(path) {
            Ok(mtime) => mtime,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot stat key ring; keeping previous keys");
                return self.snapshot();
            }
        };
        {
            let state = self.0.state.read().unwrap_or_else(|e| e.into_inner());
            if state.0 == Some(mtime) {
                return Arc::clone(&state.1);
            }
        }
        // 書きロックの下で stat し直し、記録する mtime と読む中身を同じ stat に揃える。
        // 同時に変化を見た他のリクエストは、ここで記録済みの mtime を見て読み直さない。
        let mut state = self.0.state.write().unwrap_or_else(|e| e.into_inner());
        match mtime_of(path) {
            Ok(mtime) if state.0 == Some(mtime) => {}
            // 失敗しても見た mtime は記録する (表は前の束のまま)。同じ mtime の間は
            // 読み直しも警告も繰り返さず、書き換わって mtime が変われば読み直す。
            Ok(mtime) => match read_keys(path) {
                Ok(keys) => *state = (Some(mtime), Arc::new(keys)),
                Err(error) => {
                    state.0 = Some(mtime);
                    tracing::warn!(path = %path.display(), %error, "cannot reload key ring; keeping previous keys")
                }
            },
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %unreadable(path, error), "cannot reload key ring; keeping previous keys")
            }
        }
        Arc::clone(&state.1)
    }
}

/// 置き場と、その時点の表で比べる。
impl PartialEq for KeyRing {
    fn eq(&self, other: &Self) -> bool {
        self.0.path == other.0.path && self.snapshot() == other.snapshot()
    }
}

impl Eq for KeyRing {}

impl fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyRing")
            .field("path", &self.0.path)
            .field("kids", &self.kids())
            .finish()
    }
}

fn mtime_of(path: &Path) -> Result<u64, std::io::Error> {
    let modified = fs::metadata(path)?.modified()?;
    let since_epoch = modified
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?;
    Ok(since_epoch.as_nanos() as u64)
}

fn unreadable(path: &Path, e: std::io::Error) -> String {
    format!(
        "keys_file `{}`: {e}; check the file and its permissions",
        path.display()
    )
}

/// mtime を取ってから中身を読む。間に書き換えがあっても、記録する mtime が古い側に
/// なるだけで、次の [`KeyRing::current`] で読み直される。
fn load(path: &Path) -> Result<(u64, BTreeMap<String, VerifyingKey>), String> {
    let mtime = mtime_of(path).map_err(|e| unreadable(path, e))?;
    Ok((mtime, read_keys(path)?))
}

fn read_keys(path: &Path) -> Result<BTreeMap<String, VerifyingKey>, String> {
    let text = fs::read_to_string(path).map_err(|e| unreadable(path, e))?;
    Ok(read_key_ring(&text)
        .map_err(|e| format!("keys_file `{}`: {e}; fix the key ring", path.display()))?
        .into_iter()
        .map(|(kid, key)| (kid, key.verifying_key()))
        .collect())
}

/// `jwt` 方式の設定。
///
/// 鍵束 ([`KeyRing`]) の出どころは知らない。署名・claim の検査は、入口で取った
/// その時点の表に対して行う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtAuth {
    /// kid → 公開鍵。今の方式 (alg) は EdDSA だけ。
    pub ring: KeyRing,
    /// 受ける寿命の上限 (秒)。`exp - now` (と `iat` があれば `exp - iat`) で測る。
    pub max_ttl_secs: i64,
    /// 書いたら `iss` の一致を要求する。
    pub iss: Option<String>,
    /// 書いたら `aud` (文字列か配列) に含まれることを要求する。
    pub aud: Option<String>,
}

impl JwtAuth {
    pub fn from_file(
        path: PathBuf,
        max_ttl_secs: i64,
        iss: Option<String>,
        aud: Option<String>,
    ) -> Result<Self, String> {
        Ok(Self {
            ring: KeyRing::from_file(path)?,
            max_ttl_secs,
            iss,
            aud,
        })
    }

    pub fn from_keys(
        keys: BTreeMap<String, VerifyingKey>,
        max_ttl_secs: i64,
        iss: Option<String>,
        aud: Option<String>,
    ) -> Self {
        Self {
            ring: KeyRing::from_keys(keys),
            max_ttl_secs,
            iss,
            aud,
        }
    }
}

impl JwtAuth {
    /// token を検査する。`now_secs` は unix 秒。
    ///
    /// 順序: 形 → ヘッダ (kid / alg / crit) → 署名 → claim。claim は署名が通って
    /// から読む (誰が書いたか分からない値を解釈しない)。
    pub fn verify(&self, token: &str, now_secs: i64) -> Result<Verified, Reason> {
        self.verify_with(&self.ring.current(), token, now_secs)
    }

    /// 与えた表で検査する。鍵束の読み直しとは切り離してある。
    fn verify_with(
        &self,
        keys: &BTreeMap<String, VerifyingKey>,
        token: &str,
        now_secs: i64,
    ) -> Result<Verified, Reason> {
        let mut parts = token.split('.');
        let (Some(header_b64), Some(payload_b64), Some(sig_b64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Reason::Malformed);
        };

        let header = decode_json(header_b64)?;
        let kid = header
            .get("kid")
            .and_then(Value::as_str)
            .ok_or(Reason::UnknownKid)?;
        let key = keys.get(kid).ok_or(Reason::UnknownKid)?;
        if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
            return Err(Reason::AlgMismatch);
        }
        if header.get("crit").is_some() {
            return Err(Reason::Crit);
        }

        let sig: [u8; 64] = B64
            .decode(sig_b64)
            .map_err(|_| Reason::Malformed)?
            .try_into()
            .map_err(|_| Reason::BadSignature)?;
        // 署名の入力は受け取った先頭 2 段の生のバイト列 (再エンコードしない)。
        let signed = &token[..header_b64.len() + 1 + payload_b64.len()];
        key.verify_strict(signed.as_bytes(), &Signature::from_bytes(&sig))
            .map_err(|_| Reason::BadSignature)?;

        // 時刻の計算は i128 で行う。claim は署名済みでも任意の i64 を持てるので、
        // i64 のまま足し引きすると端の値で桁あふれし、判定が逆転しうる。
        let claims = decode_json(payload_b64)?;
        let time = |name: &str| -> Result<Option<i128>, Reason> {
            match claims.get(name) {
                None => Ok(None),
                Some(v) => v
                    .as_i64()
                    .map(|t| Some(i128::from(t)))
                    .ok_or(Reason::Malformed),
            }
        };
        let (now, skew, max_ttl) = (
            i128::from(now_secs),
            i128::from(SKEW_SECS),
            i128::from(self.max_ttl_secs),
        );
        let exp = time("exp")?.ok_or(Reason::Malformed)?;
        if now > exp + skew {
            return Err(Reason::Expired);
        }
        if exp - now > max_ttl {
            return Err(Reason::TtlExceeded);
        }
        if let Some(iat) = time("iat")? {
            if iat > now + skew {
                return Err(Reason::NotYetValid);
            }
            if exp - iat > max_ttl {
                return Err(Reason::TtlExceeded);
            }
        }
        if let Some(nbf) = time("nbf")?
            && now + skew < nbf
        {
            return Err(Reason::NotYetValid);
        }
        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .ok_or(Reason::Malformed)?;
        if subject.is_empty()
            || subject.len() > MAX_SUBJECT_LEN
            || subject.chars().any(char::is_control)
        {
            return Err(Reason::Claims);
        }
        if let Some(iss) = &self.iss
            && claims.get("iss").and_then(Value::as_str) != Some(iss.as_str())
        {
            return Err(Reason::Claims);
        }
        if let Some(aud) = &self.aud {
            let named = match claims.get("aud") {
                Some(Value::String(one)) => one == aud,
                Some(Value::Array(many)) => many.iter().any(|v| v.as_str() == Some(aud)),
                _ => false,
            };
            if !named {
                return Err(Reason::Claims);
            }
        }
        Ok(Verified {
            subject: subject.to_owned(),
            kid: kid.to_owned(),
        })
    }
}

fn decode_json(part: &str) -> Result<Value, Reason> {
    let bytes = B64.decode(part).map_err(|_| Reason::Malformed)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Reason::Malformed)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(Reason::Malformed)
    }
}

/// JWS compact で署名する (`alg = EdDSA`, `kid`)。`claims` は JSON のオブジェクト。
///
/// 検証 ([`JwtAuth::verify`]) と同じ規約で作るので、鋳造した token はそのまま
/// 検証を通る。
pub fn sign(key: &ed25519_dalek::SigningKey, kid: &str, claims: &Value) -> String {
    use ed25519_dalek::Signer as _;
    let header = serde_json::json!({"alg": "EdDSA", "typ": "JWT", "kid": kid});
    let head = B64.encode(serde_json::to_vec(&header).unwrap_or_default());
    let body = B64.encode(serde_json::to_vec(claims).unwrap_or_default());
    let input = format!("{head}.{body}");
    let sig = key.sign(input.as_bytes());
    format!("{input}.{}", B64.encode(sig.to_bytes()))
}

/// 32 バイトの乱数から秘密鍵を作る。乱数は呼び出し側が OS の生成器から取る。
pub fn signing_key(seed: &[u8; 32]) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(seed)
}

/// 公開鍵を JWK の `x` の形 (32 バイトの base64url、パディング無し) にする。
pub fn public_key_text(key: &VerifyingKey) -> String {
    B64.encode(key.to_bytes())
}

/// 秘密鍵の JWK (`{"kty":"OKP","crv":"Ed25519","kid":..,"d":..,"x":..}`)。
pub fn private_jwk(kid: &str, key: &ed25519_dalek::SigningKey) -> Value {
    serde_json::json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "kid": kid,
        "d": B64.encode(key.to_bytes()),
        "x": public_key_text(&key.verifying_key()),
    })
}

/// 公開鍵の JWK (`{"kty":"OKP","crv":"Ed25519","kid":..,"alg":"EdDSA","x":..}`)。
pub fn public_jwk(kid: &str, key: &VerifyingKey) -> Value {
    serde_json::json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "kid": kid,
        "alg": "EdDSA",
        "use": "sig",
        "x": public_key_text(key),
    })
}

/// 秘密鍵の JWK を読む。`kid` と鍵を返す。`x` があれば `d` から求めた公開鍵と
/// 合っているかも確かめる (取り違えた 2 本を貼り合わせた JWK を弾く)。
pub fn read_private_jwk(text: &str) -> Result<(Option<String>, ed25519_dalek::SigningKey), String> {
    let jwk: Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("the key is not a JWK (JSON): {e}"))?;
    if jwk.get("kty").and_then(Value::as_str) != Some("OKP")
        || jwk.get("crv").and_then(Value::as_str) != Some("Ed25519")
    {
        return Err("the key must be an Ed25519 JWK (kty = OKP, crv = Ed25519)".to_owned());
    }
    if let Some(alg) = jwk.get("alg")
        && alg.as_str() != Some("EdDSA")
    {
        return Err(format!(
            "the JWK says alg = {alg}, but an Ed25519 key is used with EdDSA; fix or remove `alg`"
        ));
    }
    let d = jwk
        .get("d")
        .and_then(Value::as_str)
        .ok_or("the JWK has no private part (`d`); give the file `auth keygen` wrote")?;
    let d: [u8; 32] = B64
        .decode(d)
        .map_err(|e| format!("`d` is not base64url: {e}"))?
        .try_into()
        .map_err(|_| "`d` must be 32 bytes".to_owned())?;
    let key = ed25519_dalek::SigningKey::from_bytes(&d);
    if let Some(x) = jwk.get("x").and_then(Value::as_str)
        && x != public_key_text(&key.verifying_key())
    {
        return Err("`x` does not belong to `d`".to_owned());
    }
    let kid = jwk.get("kid").and_then(Value::as_str).map(str::to_owned);
    Ok((kid, key))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use serde_json::json;

    pub(crate) const NOW: i64 = 1_790_158_500;

    pub(crate) fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// ヘッダと claim を署名した token を作る。
    pub(crate) fn sign(key: &SigningKey, header: &Value, claims: &Value) -> String {
        let head = B64.encode(serde_json::to_vec(header).unwrap());
        let body = B64.encode(serde_json::to_vec(claims).unwrap());
        let input = format!("{head}.{body}");
        let sig = key.sign(input.as_bytes());
        format!("{input}.{}", B64.encode(sig.to_bytes()))
    }

    pub(crate) fn auth() -> JwtAuth {
        JwtAuth::from_keys(
            [("k1".to_owned(), signing_key(1).verifying_key())].into(),
            86_400,
            None,
            None,
        )
    }

    fn header() -> Value {
        json!({"alg": "EdDSA", "kid": "k1", "typ": "JWT"})
    }

    fn claims() -> Value {
        json!({"sub": "mbp", "exp": NOW + 3600, "iat": NOW})
    }

    #[test]
    fn a_signed_token_round_trips() {
        let token = sign(&signing_key(1), &header(), &claims());
        assert_eq!(
            auth().verify(&token, NOW),
            Ok(Verified {
                subject: "mbp".into(),
                kid: "k1".into()
            })
        );
    }

    #[test]
    fn a_tampered_payload_is_refused() {
        let token = sign(&signing_key(1), &header(), &claims());
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged =
            B64.encode(serde_json::to_vec(&json!({"sub": "root", "exp": NOW + 3600})).unwrap());
        parts[1] = &forged;
        assert_eq!(
            auth().verify(&parts.join("."), NOW),
            Err(Reason::BadSignature)
        );
    }

    #[test]
    fn a_token_signed_by_another_key_is_refused() {
        let token = sign(&signing_key(2), &header(), &claims());
        assert_eq!(auth().verify(&token, NOW), Err(Reason::BadSignature));
    }

    #[test]
    fn an_expired_token_is_refused_after_the_skew() {
        let c = json!({"sub": "mbp", "exp": NOW - SKEW_SECS});
        let token = sign(&signing_key(1), &header(), &c);
        assert!(auth().verify(&token, NOW).is_ok(), "within the skew");
        assert_eq!(auth().verify(&token, NOW + 1), Err(Reason::Expired));
    }

    #[test]
    fn an_unknown_kid_is_refused() {
        let token = sign(
            &signing_key(1),
            &json!({"alg": "EdDSA", "kid": "other"}),
            &claims(),
        );
        assert_eq!(auth().verify(&token, NOW), Err(Reason::UnknownKid));
        let token = sign(&signing_key(1), &json!({"alg": "EdDSA"}), &claims());
        assert_eq!(auth().verify(&token, NOW), Err(Reason::UnknownKid));
    }

    /// ヘッダの alg を書き換えても、検証の方法は変わらず、照合で断る。
    #[test]
    fn a_rewritten_alg_is_refused() {
        for alg in ["RS256", "HS256", "none", "ES256"] {
            let token = sign(
                &signing_key(1),
                &json!({"alg": alg, "kid": "k1"}),
                &claims(),
            );
            assert_eq!(
                auth().verify(&token, NOW),
                Err(Reason::AlgMismatch),
                "{alg}"
            );
        }
        // 署名欄を空にした `none` 形も通らない。
        let token = sign(
            &signing_key(1),
            &json!({"alg": "none", "kid": "k1"}),
            &claims(),
        );
        let unsigned = format!("{}.", token.rsplit_once('.').unwrap().0);
        assert!(auth().verify(&unsigned, NOW).is_err());
    }

    /// 時刻の claim が i64 の端の値でも、桁あふれせずに断る。
    #[test]
    fn extreme_times_are_refused_without_overflow() {
        for c in [
            json!({"sub": "m", "exp": i64::MAX}),
            json!({"sub": "m", "exp": i64::MIN}),
            json!({"sub": "m", "exp": NOW + 10, "iat": i64::MIN}),
            json!({"sub": "m", "exp": NOW + 10, "iat": i64::MAX}),
            json!({"sub": "m", "exp": NOW + 10, "nbf": i64::MAX}),
        ] {
            let token = sign(&signing_key(1), &header(), &c);
            assert!(auth().verify(&token, NOW).is_err(), "{c}");
            // 今が端の値でも落ちない (結果は問わない。桁あふれしないことを見る)。
            let _ = auth().verify(&token, i64::MAX);
            let _ = auth().verify(&token, i64::MIN);
        }
    }

    /// 古典的な alg 取り違え: 公開鍵のバイト列を HMAC の鍵にして、HS256 で正しく
    /// 署名した token。検証の方法は鍵の型で決まるので、署名を見る前に断る。
    #[test]
    fn an_hs256_token_keyed_with_the_public_key_is_refused() {
        use sha2::{Digest as _, Sha256};
        fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
            let mut block = [0u8; 64];
            if key.len() > 64 {
                block[..32].copy_from_slice(&Sha256::digest(key));
            } else {
                block[..key.len()].copy_from_slice(key);
            }
            let pad = |byte: u8| block.map(|b| b ^ byte);
            let inner = Sha256::new()
                .chain_update(pad(0x36))
                .chain_update(message)
                .finalize();
            Sha256::new()
                .chain_update(pad(0x5c))
                .chain_update(inner)
                .finalize()
                .into()
        }
        let public = signing_key(1).verifying_key();
        let head = B64.encode(br#"{"alg":"HS256","kid":"k1"}"#);
        let body = B64.encode(serde_json::to_vec(&claims()).unwrap());
        let input = format!("{head}.{body}");
        for key in [
            public.to_bytes().to_vec(),
            public_key_text(&public).into_bytes(),
        ] {
            let token = format!(
                "{input}.{}",
                B64.encode(hmac_sha256(&key, input.as_bytes()))
            );
            assert_eq!(auth().verify(&token, NOW), Err(Reason::AlgMismatch));
        }
    }

    #[test]
    fn crit_is_refused() {
        let token = sign(
            &signing_key(1),
            &json!({"alg": "EdDSA", "kid": "k1", "crit": ["b64"]}),
            &claims(),
        );
        assert_eq!(auth().verify(&token, NOW), Err(Reason::Crit));
    }

    #[test]
    fn the_lifetime_is_capped() {
        let c = json!({"sub": "mbp", "exp": NOW + 86_400 + 1});
        let token = sign(&signing_key(1), &header(), &c);
        assert_eq!(auth().verify(&token, NOW), Err(Reason::TtlExceeded));
        let c = json!({"sub": "mbp", "exp": NOW + 10, "iat": NOW - 86_400});
        let token = sign(&signing_key(1), &header(), &c);
        assert_eq!(
            auth().verify(&token, NOW),
            Err(Reason::TtlExceeded),
            "exp - iat"
        );
    }

    #[test]
    fn a_future_iat_or_nbf_is_refused_past_the_skew() {
        let at = |iat: i64| json!({"sub": "mbp", "exp": NOW + 3600, "iat": iat});
        let ok = sign(&signing_key(1), &header(), &at(NOW + SKEW_SECS));
        assert!(auth().verify(&ok, NOW).is_ok());
        let ahead = sign(&signing_key(1), &header(), &at(NOW + SKEW_SECS + 1));
        assert_eq!(auth().verify(&ahead, NOW), Err(Reason::NotYetValid));
        let nbf = json!({"sub": "mbp", "exp": NOW + 3600, "nbf": NOW + SKEW_SECS + 1});
        let token = sign(&signing_key(1), &header(), &nbf);
        assert_eq!(auth().verify(&token, NOW), Err(Reason::NotYetValid));
    }

    #[test]
    fn required_claims_and_configured_iss_aud() {
        for c in [
            json!({"exp": NOW + 10}),
            json!({"sub": "mbp"}),
            json!({"sub": "", "exp": NOW + 10}),
            json!({"sub": "a\nb", "exp": NOW + 10}),
        ] {
            let token = sign(&signing_key(1), &header(), &c);
            assert!(auth().verify(&token, NOW).is_err(), "{c}");
        }
        let strict = JwtAuth {
            iss: Some("cli".into()),
            aud: Some("ns-a".into()),
            ..auth()
        };
        let good = json!({"sub": "m", "exp": NOW + 10, "iss": "cli", "aud": ["x", "ns-a"]});
        assert!(
            strict
                .verify(&sign(&signing_key(1), &header(), &good), NOW)
                .is_ok()
        );
        let wrong = json!({"sub": "m", "exp": NOW + 10, "iss": "cli", "aud": "x"});
        assert_eq!(
            strict.verify(&sign(&signing_key(1), &header(), &wrong), NOW),
            Err(Reason::Claims)
        );
    }

    #[test]
    fn malformed_tokens_are_refused() {
        for bad in ["", "a.b", "a.b.c.d", "!!.!!.!!", "e30.e30.e30"] {
            assert!(auth().verify(bad, NOW).is_err(), "{bad:?}");
        }
    }

    /// ここで鋳造した token は、同じ規約の検証を通る。
    #[test]
    fn a_minted_token_passes_verification() {
        let key = signing_key(1);
        let jwk = private_jwk("k1", &key).to_string();
        let (kid, read) = read_private_jwk(&jwk).unwrap();
        assert_eq!(kid.as_deref(), Some("k1"));
        let token = super::sign(&read, "k1", &json!({"sub": "mbp", "exp": NOW + 60}));
        assert!(auth().verify(&token, NOW).is_ok());
        let mut mixed: Value = serde_json::from_str(&jwk).unwrap();
        mixed["x"] = json!(public_key_text(&signing_key(2).verifying_key()));
        assert!(read_private_jwk(&mixed.to_string()).is_err());
    }

    #[test]
    fn key_ring_reads_lines_and_rejects_invalid_entries() {
        let first = private_jwk("a", &signing_key(1)).to_string();
        let second = private_jwk("b", &signing_key(2)).to_string();
        let ring = read_key_ring(&format!("{first}\n\n{second}\n")).unwrap();
        assert_eq!(ring.len(), 2);
        assert_eq!(ring["a"].verifying_key(), signing_key(1).verifying_key());
        assert_eq!(ring["b"].verifying_key(), signing_key(2).verifying_key());

        let mut wrong_x: Value = serde_json::from_str(&first).unwrap();
        wrong_x["x"] = json!(public_key_text(&signing_key(2).verifying_key()));
        let mut no_kid: Value = serde_json::from_str(&first).unwrap();
        no_kid.as_object_mut().unwrap().remove("kid");
        let mut no_private: Value = serde_json::from_str(&first).unwrap();
        no_private.as_object_mut().unwrap().remove("d");
        let mut wrong_alg: Value = serde_json::from_str(&first).unwrap();
        wrong_alg["alg"] = json!("RS256");
        let mut wrong_curve: Value = serde_json::from_str(&first).unwrap();
        wrong_curve["crv"] = json!("X25519");
        for bad in [
            "not JSON".to_owned(),
            wrong_curve.to_string(),
            no_private.to_string(),
            wrong_x.to_string(),
            no_kid.to_string(),
            wrong_alg.to_string(),
            first.clone(),
        ] {
            let error = read_key_ring(&format!("{first}\n\n{bad}")).unwrap_err();
            assert!(error.contains("line 3"), "{error}");
            assert!(!error.contains("\"d\":"), "{error}");
        }
    }

    /// `alg` の申告は、あれば EdDSA との一致を要求する (方式は kty / crv で決まる)。
    #[test]
    fn a_declared_alg_must_be_eddsa() {
        let mut jwk = private_jwk("a", &signing_key(1));
        jwk["alg"] = json!("EdDSA");
        assert!(read_key_ring(&jwk.to_string()).is_ok());
        jwk["alg"] = json!("ES256");
        let error = read_key_ring(&format!("\n{jwk}")).unwrap_err();
        assert!(error.contains("line 2") && error.contains("alg"), "{error}");
    }

    /// clone は状態を共有する。片方で読み直せば、もう片方も新しい表を返す。
    #[test]
    fn a_cloned_key_ring_shares_the_reload() {
        use std::fs::{self, File, FileTimes};
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring.jsonl");
        let write_at = |text: &str, seconds: u64| {
            fs::write(&path, text).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    FileTimes::new()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                )
                .unwrap();
        };
        write_at(&private_jwk("a", &signing_key(1)).to_string(), 100);
        let ring = KeyRing::from_file(path.clone()).unwrap();
        let other = ring.clone();
        write_at(&private_jwk("b", &signing_key(2)).to_string(), 101);
        assert_eq!(ring.current().keys().collect::<Vec<_>>(), ["b"]);
        assert_eq!(
            other.kids(),
            ["b"],
            "the clone sees the reload without reading"
        );
    }

    #[test]
    fn an_empty_key_ring_is_an_error() {
        for text in ["", "\n", "  \n\n"] {
            let error = read_key_ring(text).unwrap_err();
            assert!(error.contains("no keys"), "{error}");
        }
    }

    #[test]
    fn key_ring_reloads_on_mtime_change_and_keeps_last_valid_ring() {
        use std::fs::{self, File, FileTimes};
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring.jsonl");
        let first = private_jwk("a", &signing_key(1)).to_string();
        let second = private_jwk("b", &signing_key(2)).to_string();
        let write_at = |text: &str, seconds: u64| {
            fs::write(&path, text).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    FileTimes::new()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                )
                .unwrap();
        };
        let a = super::sign(&signing_key(1), "a", &claims());
        let b = super::sign(&signing_key(2), "b", &claims());
        write_at(&first, 100);
        let auth = JwtAuth::from_file(path.clone(), 86_400, None, None).unwrap();
        assert!(auth.verify(&a, NOW).is_ok());
        assert_eq!(auth.verify(&b, NOW), Err(Reason::UnknownKid));
        write_at(&format!("{first}\n{second}\n"), 101);
        assert!(auth.verify(&b, NOW).is_ok());
        write_at("not JSON", 102);
        assert!(auth.verify(&a, NOW).is_ok());
        assert!(auth.verify(&b, NOW).is_ok());
        // 失敗した mtime のままなら読み直さない (中身を正しくしても mtime が同じなら拾わない)。
        write_at(&second, 102);
        assert!(
            auth.verify(&a, NOW).is_ok(),
            "a failed mtime is not retried"
        );
        write_at(&second, 103);
        assert_eq!(auth.verify(&a, NOW), Err(Reason::UnknownKid));
        assert!(auth.verify(&b, NOW).is_ok());
        write_at("", 104);
        assert!(
            auth.verify(&b, NOW).is_ok(),
            "a truncated ring keeps the last one"
        );
        write_at("\n\n", 105);
        assert!(
            auth.verify(&b, NOW).is_ok(),
            "blank lines only keep the last one"
        );
        write_at(&second, 106);
        assert!(auth.verify(&b, NOW).is_ok());
        write_at(&first, 106);
        assert!(
            auth.verify(&b, NOW).is_ok(),
            "unchanged mtime does not reload"
        );
        assert_eq!(auth.verify(&a, NOW), Err(Reason::UnknownKid));
    }
}
