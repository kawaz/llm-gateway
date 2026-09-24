//! namespace の入口での認証。
//!
//! 検証の出口は方式によらず「Bearer → 主体 ([`Principal`])」に揃える
//! (DR-0030 §6)。下流は主体だけを見ればよく、方式が増えても変わらない。

pub mod jwt;

/// 検証を通った相手。
///
/// 固定トークンの方式では、誰が名乗ったかも、どの鍵で確かめたかも分からない
/// ので `subject` / `kid` は `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub ns: String,
    pub subject: Option<String>,
    pub kid: Option<String>,
}

/// 検証の結果。
///
/// bool で返すと「なぜ通ったか」が消える。トークンが合って通ったのと、
/// そもそも検査していないのとでは意味が違い、記録に残す価値も違う
/// (DR-0006)。列挙にしておくと `match` が網羅を強制するので、通す枝と
/// 拒む枝のどちらも書き落とせない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
    /// トークンが合っている。
    Accepted(Principal),
    /// トークンが違う (名乗っていない場合を含む)。
    WrongToken,
    /// `jwt` 方式で通らなかった。理由はログのためだけにあり、応答で区別しない。
    Rejected(jwt::Reason),
    /// この namespace は誰でも通す (トークンを設定していない)。
    Open,
}

/// namespace ごとの認証の方式。
///
/// 方式が増えても出口は [`Authorization`] の主体 ([`Principal`]) に揃える
/// (DR-0030 §6)。設定ファイルの書き方 (どの欄から組み立てるか) は利用側が持つ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NsAuth {
    /// 検査しない。**書かなければ誰でも通す** (DR-0006)。手前 (tailnet / リバース
    /// プロキシ) で境界を引く運用では、ここで二重に認証を求める意味がない。
    #[default]
    Open,
    /// この固定トークンを名乗った相手だけを通す。
    Token(String),
    /// 設定した公開鍵で署名された JWT を名乗った相手を通す。
    Jwt(jwt::JwtAuth),
}

impl NsAuth {
    /// この固定トークンを名乗った相手だけを通す。
    pub fn token(token: impl Into<String>) -> Self {
        Self::Token(token.into())
    }

    /// 検査しない (誰でも通す) 設定か。
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }

    /// クライアントが名乗った `Authorization` の値を検査する。
    ///
    /// `Bearer xxx` でも `xxx` でも受ける。クライアントによって送り方が違う。
    pub fn verify(&self, ns: &str, presented: Option<&str>) -> Authorization {
        match self {
            Self::Open => Authorization::Open,
            Self::Token(expected) => {
                let matched = presented.is_some_and(|p| {
                    let p = p.strip_prefix("Bearer ").unwrap_or(p).trim();
                    p == expected
                });
                if matched {
                    Authorization::Accepted(Principal {
                        ns: ns.to_owned(),
                        subject: None,
                        kid: None,
                    })
                } else {
                    Authorization::WrongToken
                }
            }
            Self::Jwt(auth) => {
                let Some(token) = presented.map(|p| p.strip_prefix("Bearer ").unwrap_or(p).trim())
                else {
                    return Authorization::Rejected(jwt::Reason::Malformed);
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                match auth.verify(token, now) {
                    Ok(verified) => Authorization::Accepted(Principal {
                        ns: ns.to_owned(),
                        subject: Some(verified.subject),
                        kid: Some(verified.kid),
                    }),
                    Err(reason) => Authorization::Rejected(reason),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_passes_anyone() {
        let auth = NsAuth::default();
        assert!(auth.is_open());
        for presented in [None, Some(""), Some("Bearer x")] {
            assert_eq!(auth.verify("n", presented), Authorization::Open);
        }
    }

    #[test]
    fn a_matching_token_yields_the_principal() {
        let auth = NsAuth::token("s");
        assert!(!auth.is_open());
        let expected = Authorization::Accepted(Principal {
            ns: "n".to_owned(),
            subject: None,
            kid: None,
        });
        assert_eq!(auth.verify("n", Some("Bearer s")), expected);
        assert_eq!(auth.verify("n", Some("s")), expected);
        for bad in [None, Some("Bearer t"), Some("")] {
            assert_eq!(auth.verify("n", bad), Authorization::WrongToken);
        }
    }

    /// `jwt` 方式は署名された token の主体 (sub / kid) を返し、名乗らない相手は断る。
    #[test]
    fn a_jwt_yields_its_subject_and_kid() {
        use jwt::tests::{auth, sign, signing_key};
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let token = sign(
            &signing_key(1),
            &serde_json::json!({"alg": "EdDSA", "kid": "k1"}),
            &serde_json::json!({"sub": "mbp", "exp": now + 600}),
        );
        let ns = NsAuth::Jwt(auth());
        assert!(!ns.is_open());
        assert_eq!(
            ns.verify("n", Some(&format!("Bearer {token}"))),
            Authorization::Accepted(Principal {
                ns: "n".into(),
                subject: Some("mbp".into()),
                kid: Some("k1".into()),
            })
        );
        assert!(matches!(ns.verify("n", None), Authorization::Rejected(_)));
        assert!(matches!(
            ns.verify("n", Some("Bearer nope")),
            Authorization::Rejected(jwt::Reason::Malformed)
        ));
    }
}
