use super::*;
use crate::ns::jwt::tests::{NOW, sign, signing_key};
use crate::ns::jwt::{public_jwk, public_key_text};
use serde_json::json;

const AUD: &str = "GET /ops/sessions/abc";

/// 公開鍵の JWK (`alg` は任意)。
fn jwk(kid: &str, seed: u8, alg: Option<&str>) -> Value {
    let mut jwk = json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "kid": kid,
        "x": public_key_text(&signing_key(seed).verifying_key()),
    });
    if let Some(alg) = alg {
        jwk["alg"] = json!(alg);
    }
    jwk
}

fn jwks(keys: Vec<Value>) -> PublicKeys {
    parse_jwks(
        &serde_json::to_vec(&json!({ "keys": keys })).unwrap(),
        "test",
    )
    .unwrap()
}

/// 呼び手 2 つ。`shared` は両方にある kid で、鍵は別。
struct Fixture {
    /// (名前, 実効 `iss`, 鍵)。
    callers: Vec<(String, String, PublicKeys)>,
    jtis: JtiSet,
}

impl Fixture {
    fn new() -> Self {
        Self::with_jtis(JtiSet::new())
    }

    fn with_jtis(jtis: JtiSet) -> Self {
        Self {
            callers: vec![
                (
                    "alpha".into(),
                    "alpha".into(),
                    jwks(vec![
                        jwk("a1", 1, None),
                        jwk("shared", 4, None),
                        jwk("pinned", 6, Some("EdDSA")),
                    ]),
                ),
                (
                    "beta".into(),
                    "beta-iss".into(),
                    jwks(vec![jwk("b1", 2, None), jwk("shared", 5, None)]),
                ),
            ],
            jtis,
        }
    }

    fn lookup(&self, iss: &str, kid: &str) -> Result<Resolved, Missing> {
        let Some((caller, iss, keys)) = self.callers.iter().find(|c| c.1 == iss) else {
            return Err(Missing::UnknownIss);
        };
        match keys.get(kid) {
            None => Err(Missing::UnknownKid),
            Some(key) => Ok(Resolved {
                caller: caller.clone(),
                iss: iss.clone(),
                key: *key,
            }),
        }
    }

    fn verify_at(&self, authorization: Option<&str>, now: i64) -> Result<Verified, Reason> {
        verify(
            authorization.map(str::as_bytes),
            AUD,
            now,
            |iss, kid| self.lookup(iss, kid),
            &self.jtis,
        )
    }

    fn verify(&self, authorization: &str) -> Result<Verified, Reason> {
        self.verify_at(Some(authorization), NOW)
    }
}

fn header() -> Value {
    json!({"alg": "Ed25519", "typ": "request+jwt", "kid": "a1"})
}

fn claims() -> Value {
    json!({"iss": "alpha", "aud": AUD, "iat": NOW, "exp": NOW + 10})
}

/// `base` に `changes` の欄を書き足す (同じ名前は置き換える)。
fn with(mut base: Value, changes: Value) -> Value {
    for (name, value) in changes.as_object().unwrap() {
        base[name] = value.clone();
    }
    base
}

fn without(mut base: Value, name: &str) -> Value {
    base.as_object_mut().unwrap().remove(name);
    base
}

fn bearer_of(seed: u8, header: &Value, claims: &Value) -> String {
    format!("Bearer {}", sign(&signing_key(seed), header, claims))
}

fn alpha() -> Verified {
    Verified {
        caller: "alpha".into(),
        kid: "a1".into(),
    }
}

#[track_caller]
fn assert_rejected(result: Result<Verified, Reason>, reason: Reason, status: Status) {
    assert_eq!(result, Err(reason));
    assert_eq!(reason.status(), status, "{reason}");
}

#[test]
fn a_signed_request_token_names_its_caller() {
    let f = Fixture::new();
    assert_eq!(f.verify(&bearer_of(1, &header(), &claims())), Ok(alpha()));
    let eddsa = with(header(), json!({"alg": "EdDSA"}));
    assert_eq!(f.verify(&bearer_of(1, &eddsa, &claims())), Ok(alpha()));
    let beta = bearer_of(
        2,
        &with(header(), json!({"kid": "b1"})),
        &with(claims(), json!({"iss": "beta-iss"})),
    );
    assert_eq!(
        f.verify(&beta),
        Ok(Verified {
            caller: "beta".into(),
            kid: "b1".into()
        })
    );
}

#[test]
fn a_request_without_authorization_is_unauthorized() {
    assert_rejected(
        Fixture::new().verify_at(None, NOW),
        Reason::MissingAuthorization,
        Status::Unauthorized,
    );
}

#[test]
fn authorization_must_be_a_bearer_token() {
    let f = Fixture::new();
    let token = sign(&signing_key(1), &header(), &claims());
    for value in [
        "".to_owned(),
        "Bearer".to_owned(),
        "Bearer ".to_owned(),
        format!("Basic {token}"),
        format!("Bearer{token}"),
        token.clone(),
    ] {
        assert_rejected(f.verify(&value), Reason::NotBearer, Status::BadRequest);
    }
    let not_utf8 = verify(
        Some(b"Bearer \xff\xfe"),
        AUD,
        NOW,
        |_, _| unreachable!(),
        &f.jtis,
    );
    assert_rejected(not_utf8, Reason::NotBearer, Status::BadRequest);
    // scheme の大文字小文字と、間の空白の数は問わない。
    assert_eq!(f.verify(&format!("bearer {token}")), Ok(alpha()));
    assert_eq!(f.verify(&format!("BEARER   {token}")), Ok(alpha()));
}

#[test]
fn a_token_that_is_not_a_compact_jws_is_a_bad_request() {
    let f = Fixture::new();
    let token = sign(&signing_key(1), &header(), &claims());
    let (head, rest) = token.split_once('.').unwrap();
    let (body, _) = rest.split_once('.').unwrap();
    let array = B64.encode(b"[1]");
    for value in [
        "abc".to_owned(),
        "a.b".to_owned(),
        format!("{token}.extra"),
        format!("!!.{body}.AAAA"),
        format!("{array}.{body}.AAAA"),
        format!("{head}.{array}.AAAA"),
        format!("{head}.{body}.!!"),
    ] {
        assert_rejected(
            f.verify(&format!("Bearer {value}")),
            Reason::Malformed,
            Status::BadRequest,
        );
    }
}

#[test]
fn only_ed25519_and_eddsa_are_accepted_as_alg() {
    let f = Fixture::new();
    for alg in [
        json!("none"),
        json!("RS256"),
        json!("HS256"),
        json!("ES256"),
        json!("eddsa"),
        json!("ED25519"),
        json!(null),
    ] {
        let token = bearer_of(1, &with(header(), json!({ "alg": alg })), &claims());
        assert_rejected(f.verify(&token), Reason::AlgNotAccepted, Status::BadRequest);
    }
    let token = bearer_of(1, &without(header(), "alg"), &claims());
    assert_rejected(f.verify(&token), Reason::AlgNotAccepted, Status::BadRequest);
}

/// 他の用途の token (ns の `JWT`、hooks の `webhook+jwt`) は通らない。比較は正規化してから。
#[test]
fn typ_must_be_request_jwt_after_normalization() {
    let f = Fixture::new();
    for typ in [
        json!("JWT"),
        json!("webhook+jwt"),
        json!("request"),
        json!("application/jwt"),
        json!("x-request+jwt"),
        json!(1),
    ] {
        let token = bearer_of(1, &with(header(), json!({ "typ": typ })), &claims());
        assert_rejected(f.verify(&token), Reason::TypMismatch, Status::BadRequest);
    }
    let token = bearer_of(1, &without(header(), "typ"), &claims());
    assert_rejected(f.verify(&token), Reason::TypMismatch, Status::BadRequest);
    for typ in [
        "application/request+jwt",
        "REQUEST+JWT",
        "Application/Request+JWT",
    ] {
        let token = bearer_of(1, &with(header(), json!({ "typ": typ })), &claims());
        assert_eq!(f.verify(&token), Ok(alpha()), "{typ}");
    }
}

#[test]
fn a_token_without_kid_is_a_bad_request() {
    let f = Fixture::new();
    for kid in [json!(""), json!(1), json!(null)] {
        let token = bearer_of(1, &with(header(), json!({ "kid": kid })), &claims());
        assert_rejected(f.verify(&token), Reason::MissingKid, Status::BadRequest);
    }
    let token = bearer_of(1, &without(header(), "kid"), &claims());
    assert_rejected(f.verify(&token), Reason::MissingKid, Status::BadRequest);
}

#[test]
fn a_critical_extension_is_refused() {
    let token = bearer_of(1, &with(header(), json!({"crit": ["exp"]})), &claims());
    assert_rejected(
        Fixture::new().verify(&token),
        Reason::Crit,
        Status::BadRequest,
    );
}

#[test]
fn an_unknown_iss_is_a_bad_request() {
    let f = Fixture::new();
    for iss in [json!("gamma"), json!("beta"), json!(1)] {
        let token = bearer_of(1, &header(), &with(claims(), json!({ "iss": iss })));
        assert_rejected(f.verify(&token), Reason::UnknownIss, Status::BadRequest);
    }
    let token = bearer_of(1, &header(), &without(claims(), "iss"));
    assert_rejected(f.verify(&token), Reason::UnknownIss, Status::BadRequest);
}

/// kid は名乗った呼び手の鍵だけから引く。別の呼び手の kid は無いのと同じ。
#[test]
fn an_unknown_kid_is_unauthorized() {
    let f = Fixture::new();
    let token = bearer_of(1, &with(header(), json!({"kid": "zz"})), &claims());
    assert_rejected(f.verify(&token), Reason::UnknownKid, Status::Unauthorized);
    let token = bearer_of(2, &with(header(), json!({"kid": "b1"})), &claims());
    assert_rejected(f.verify(&token), Reason::UnknownKid, Status::Unauthorized);
}

/// 別の呼び手の `iss` を名乗った token は、名乗った呼び手の鍵で検証されて落ちる。
#[test]
fn a_token_claiming_another_callers_iss_fails_on_that_callers_key() {
    let token = bearer_of(
        4,
        &with(header(), json!({"kid": "shared"})),
        &with(claims(), json!({"iss": "beta-iss"})),
    );
    assert_rejected(
        Fixture::new().verify(&token),
        Reason::BadSignature,
        Status::Unauthorized,
    );
}

#[test]
fn stale_keys_are_unauthorized() {
    let f = Fixture::new();
    let token = bearer_of(1, &header(), &claims());
    let result = verify(
        Some(token.as_bytes()),
        AUD,
        NOW,
        |_, _| Err(Missing::Stale),
        &f.jtis,
    );
    assert_rejected(result, Reason::KeysStale, Status::Unauthorized);
}

/// JWK に `alg` があれば header の `alg` と一致を見る。
#[test]
fn the_jwk_alg_must_match_the_header_alg() {
    let f = Fixture::new();
    let pinned = with(header(), json!({"kid": "pinned"}));
    let token = bearer_of(6, &pinned, &claims());
    assert_rejected(f.verify(&token), Reason::KeyAlgMismatch, Status::BadRequest);
    let token = bearer_of(6, &with(pinned, json!({"alg": "EdDSA"})), &claims());
    assert_eq!(
        f.verify(&token),
        Ok(Verified {
            caller: "alpha".into(),
            kid: "pinned".into()
        })
    );
}

#[test]
fn a_wrong_signature_is_unauthorized() {
    let f = Fixture::new();
    let other_key = bearer_of(2, &header(), &claims());
    assert_rejected(
        f.verify(&other_key),
        Reason::BadSignature,
        Status::Unauthorized,
    );

    let token = sign(&signing_key(1), &header(), &claims());
    let parts: Vec<&str> = token.split('.').collect();
    let forged =
        B64.encode(serde_json::to_vec(&with(claims(), json!({"aud": "GET /other"}))).unwrap());
    let tampered = format!("Bearer {}.{forged}.{}", parts[0], parts[2]);
    assert_rejected(
        f.verify(&tampered),
        Reason::BadSignature,
        Status::Unauthorized,
    );
    let short = format!("Bearer {}.{}.{}", parts[0], parts[1], &parts[2][..40]);
    assert_rejected(f.verify(&short), Reason::BadSignature, Status::Unauthorized);
}

/// `now` < `exp` だけを見て、猶予を足さない。
#[test]
fn a_token_is_expired_at_exp() {
    let f = Fixture::new();
    let token = bearer_of(1, &header(), &claims());
    assert_eq!(f.verify_at(Some(&token), NOW + 9), Ok(alpha()));
    assert_rejected(
        f.verify_at(Some(&token), NOW + 10),
        Reason::Expired,
        Status::Unauthorized,
    );
}

#[test]
fn the_lifetime_must_be_within_ten_seconds() {
    let f = Fixture::new();
    let ten = bearer_of(1, &header(), &with(claims(), json!({"exp": NOW + 10})));
    assert_eq!(f.verify(&ten), Ok(alpha()));
    for (iat, exp) in [(NOW, NOW + 11), (NOW - 100, NOW + 100), (NOW + 5, NOW + 3)] {
        let token = bearer_of(
            1,
            &header(),
            &with(claims(), json!({"iat": iat, "exp": exp})),
        );
        assert_rejected(f.verify(&token), Reason::Lifetime, Status::BadRequest);
    }
}

#[test]
fn iat_and_exp_are_required() {
    let f = Fixture::new();
    for name in ["iat", "exp"] {
        let token = bearer_of(1, &header(), &without(claims(), name));
        assert_rejected(f.verify(&token), Reason::MissingTime, Status::BadRequest);
    }
}

/// 時刻は 0 以上の整数 (i64)。小数・負・文字列・i64 を超える値は断り、端の値でも桁あふれしない。
#[test]
fn times_must_be_non_negative_integers() {
    let f = Fixture::new();
    for value in [
        json!("1790158500"),
        json!(1.5),
        json!(-1),
        json!(1e30),
        json!(u64::MAX),
        json!(null),
        json!(true),
    ] {
        for name in ["iat", "exp"] {
            let token = bearer_of(1, &header(), &with(claims(), json!({ name: value })));
            assert_rejected(f.verify(&token), Reason::InvalidTime, Status::BadRequest);
        }
    }
    for (iat, exp) in [(0, i64::MAX), (i64::MAX, 0)] {
        let token = bearer_of(
            1,
            &header(),
            &with(claims(), json!({"iat": iat, "exp": exp})),
        );
        assert_rejected(f.verify(&token), Reason::Lifetime, Status::BadRequest);
        let _ = f.verify_at(Some(&token), i64::MAX);
        let _ = f.verify_at(Some(&token), i64::MIN);
    }
}

/// `iat` が先の時刻でも断らない (ログに残すだけ)。`nbf` は見ない。
#[test]
fn a_future_iat_and_nbf_are_not_checked() {
    let f = Fixture::new();
    let ahead = bearer_of(
        1,
        &header(),
        &with(claims(), json!({"iat": NOW + 100, "exp": NOW + 105})),
    );
    assert_eq!(f.verify(&ahead), Ok(alpha()));
    let nbf = bearer_of(1, &header(), &with(claims(), json!({"nbf": NOW + 1000})));
    assert_eq!(f.verify(&nbf), Ok(alpha()));
}

#[test]
fn aud_must_be_the_method_and_path() {
    let f = Fixture::new();
    for aud in [
        json!("GET /ops/sessions"),
        json!("get /ops/sessions/abc"),
        json!("POST /ops/sessions/abc"),
        json!("GET /ops/sessions/abc?ns=x"),
        json!("GET  /ops/sessions/abc"),
        json!("https://gw.example/ops/sessions/abc"),
        json!([AUD]),
    ] {
        let token = bearer_of(1, &header(), &with(claims(), json!({ "aud": aud })));
        assert_rejected(f.verify(&token), Reason::AudMismatch, Status::BadRequest);
    }
    let token = bearer_of(1, &header(), &without(claims(), "aud"));
    assert_rejected(f.verify(&token), Reason::AudMismatch, Status::BadRequest);
    assert_eq!(audience("GET", "/ops/sessions/abc"), AUD);
}

/// 引いた呼び手の `iss` と署名済みの `iss` を改めて比べる (引き方が `iss` を取り違えても通さない)。
#[test]
fn iss_is_compared_again_after_the_signature() {
    let f = Fixture::new();
    let token = bearer_of(1, &header(), &claims());
    let key = *f.callers[0].2.get("a1").unwrap();
    let result = verify(
        Some(token.as_bytes()),
        AUD,
        NOW,
        |_, _| {
            Ok(Resolved {
                caller: "alpha".into(),
                iss: "someone-else".into(),
                key,
            })
        },
        &f.jtis,
    );
    assert_rejected(result, Reason::IssMismatch, Status::BadRequest);
}

#[test]
fn a_reused_jti_is_unauthorized() {
    let f = Fixture::new();
    let token = bearer_of(1, &header(), &with(claims(), json!({"jti": "j1"})));
    assert_eq!(f.verify(&token), Ok(alpha()));
    assert_rejected(f.verify(&token), Reason::JtiReused, Status::Unauthorized);
    // 別の呼び手が同じ jti を使っても、互いに使用済みにしない。
    let beta = bearer_of(
        2,
        &with(header(), json!({"kid": "b1"})),
        &with(claims(), json!({"iss": "beta-iss", "jti": "j1"})),
    );
    assert!(f.verify(&beta).is_ok());
}

#[test]
fn a_token_without_jti_is_accepted_and_not_recorded() {
    let f = Fixture::new();
    let token = bearer_of(1, &header(), &claims());
    assert_eq!(f.verify(&token), Ok(alpha()));
    assert_eq!(f.verify(&token), Ok(alpha()));
    assert!(f.jtis.is_empty());
}

#[test]
fn a_non_string_jti_is_a_bad_request() {
    let f = Fixture::new();
    for jti in [json!(1), json!(null), json!(["j"])] {
        let token = bearer_of(1, &header(), &with(claims(), json!({ "jti": jti })));
        assert_rejected(f.verify(&token), Reason::InvalidJti, Status::BadRequest);
    }
}

/// 満杯の間は jti を見ずに通す (拒否しない、記録もしない)。期限を過ぎた記録が掃かれれば、また記録して見る。
#[test]
fn a_full_jti_set_skips_the_check() {
    let f = Fixture::with_jtis(JtiSet::with_capacity(2));
    let at = |jti: &str, iat: i64| {
        bearer_of(
            1,
            &header(),
            &with(claims(), json!({"jti": jti, "iat": iat, "exp": iat + 10})),
        )
    };
    assert_eq!(f.verify(&at("one", NOW)), Ok(alpha()));
    assert_eq!(f.verify(&at("two", NOW)), Ok(alpha()));
    assert_eq!(f.verify(&at("three", NOW)), Ok(alpha()));
    assert_eq!(
        f.verify(&at("three", NOW)),
        Ok(alpha()),
        "not recorded while full"
    );
    assert_eq!(
        f.verify(&at("one", NOW)),
        Ok(alpha()),
        "not looked up while full"
    );
    assert_eq!(f.jtis.len(), 2);

    assert_eq!(
        f.verify_at(Some(&at("three", NOW + 10)), NOW + 10),
        Ok(alpha())
    );
    assert_eq!(f.jtis.len(), 1, "the expired records were swept");
    assert_rejected(
        f.verify_at(Some(&at("three", NOW + 10)), NOW + 11),
        Reason::JtiReused,
        Status::Unauthorized,
    );
}

#[test]
fn jti_records_are_kept_until_exp() {
    let set = JtiSet::with_capacity(2);
    assert_eq!(set.record("c", "j", NOW + 10, NOW), JtiOutcome::Recorded);
    assert_eq!(set.record("c", "j", NOW + 10, NOW + 9), JtiOutcome::Reused);
    assert_eq!(
        set.record("d", "j", NOW + 10, NOW + 9),
        JtiOutcome::Recorded
    );
    assert_eq!(set.record("e", "j", NOW + 10, NOW + 9), JtiOutcome::Skipped);
    // exp に達した記録は掃かれ、同じ jti をまた記録できる。
    assert_eq!(
        set.record("c", "j", NOW + 20, NOW + 10),
        JtiOutcome::Recorded
    );
    assert_eq!(set.len(), 1);
}

/// header の鍵の在り処 (`jku` / `jwk` / `x5u` / `x5c`) は読まない。
#[test]
fn key_locations_in_the_header_are_ignored() {
    let f = Fixture::new();
    let planted = with(
        header(),
        json!({
            "jwk": jwk("a1", 9, None),
            "jku": "https://attacker.example/jwks.json",
            "x5u": "https://attacker.example/cert.pem",
            "x5c": ["AAAA"],
        }),
    );
    let attacker = bearer_of(9, &planted, &claims());
    assert_rejected(
        f.verify(&attacker),
        Reason::BadSignature,
        Status::Unauthorized,
    );
    assert_eq!(f.verify(&bearer_of(1, &planted, &claims())), Ok(alpha()));
}

/// 1 つ落ちたらそこで終わる。鍵は段 3 まで引かず、jti は段 6 を通るまで記録しない。
#[test]
fn verification_stops_at_the_first_failure() {
    let f = Fixture::new();
    let bad_typ_and_iss = bearer_of(
        1,
        &with(header(), json!({"typ": "JWT"})),
        &with(claims(), json!({"iss": "gamma"})),
    );
    let result = verify(
        Some(bad_typ_and_iss.as_bytes()),
        AUD,
        NOW,
        |_, _| unreachable!("the key is looked up only after the header"),
        &f.jtis,
    );
    assert_rejected(result, Reason::TypMismatch, Status::BadRequest);

    let forged_and_expired = bearer_of(2, &header(), &claims());
    assert_rejected(
        f.verify_at(Some(&forged_and_expired), NOW + 100),
        Reason::BadSignature,
        Status::Unauthorized,
    );

    let expired_elsewhere = bearer_of(1, &header(), &with(claims(), json!({"aud": "GET /other"})));
    assert_rejected(
        f.verify_at(Some(&expired_elsewhere), NOW + 100),
        Reason::Expired,
        Status::Unauthorized,
    );

    let elsewhere = bearer_of(
        1,
        &header(),
        &with(claims(), json!({"aud": "GET /other", "jti": "j"})),
    );
    assert_rejected(
        f.verify(&elsewhere),
        Reason::AudMismatch,
        Status::BadRequest,
    );
    assert!(f.jtis.is_empty());
}

/// 401 は署名し直せば通りうるもの、400 は直しても通らないもの (DR-0043 決定 4 の表)。
#[test]
fn every_reason_has_the_status_of_the_decision_table() {
    use Reason::*;
    for reason in [
        MissingAuthorization,
        UnknownKid,
        KeysStale,
        BadSignature,
        Expired,
        JtiReused,
    ] {
        assert_eq!(reason.status(), Status::Unauthorized, "{reason}");
    }
    for reason in [
        NotBearer,
        Malformed,
        AlgNotAccepted,
        TypMismatch,
        MissingKid,
        Crit,
        UnknownIss,
        KeyAlgMismatch,
        MissingTime,
        InvalidTime,
        Lifetime,
        AudMismatch,
        IssMismatch,
        InvalidJti,
    ] {
        assert_eq!(reason.status(), Status::BadRequest, "{reason}");
    }
}

#[test]
fn the_jwks_parser_takes_only_usable_keys() {
    let x = |seed: u8| public_key_text(&signing_key(seed).verifying_key());
    let mut with_private = jwk("with-d", 7, None);
    with_private["d"] = json!(B64.encode(signing_key(7).to_bytes()));
    let mut foreign_private = jwk("foreign-d", 8, None);
    foreign_private["d"] = json!(B64.encode(signing_key(7).to_bytes()));
    let body = json!({"keys": [
        jwk("plain", 1, None),
        jwk("alg-ed25519", 2, Some("Ed25519")),
        public_jwk("alg-eddsa", &signing_key(3).verifying_key()),
        with_private,
        foreign_private,
        {"kty": "RSA", "kid": "rsa", "n": "AQAB", "e": "AQAB"},
        {"kty": "OKP", "crv": "X25519", "kid": "x25519", "x": x(1)},
        {"kty": "OKP", "crv": "Ed25519", "kid": "enc", "use": "enc", "x": x(1)},
        {"kty": "OKP", "crv": "Ed25519", "kid": "use-number", "use": 1, "x": x(1)},
        jwk("es256", 1, Some("ES256")),
        {"kty": "OKP", "crv": "Ed25519", "kid": "alg-number", "alg": 1, "x": x(1)},
        {"kty": "OKP", "crv": "Ed25519", "x": x(1)},
        {"kty": "OKP", "crv": "Ed25519", "kid": "", "x": x(1)},
        {"kty": "OKP", "crv": "Ed25519", "kid": "no-x"},
        {"kty": "OKP", "crv": "Ed25519", "kid": "bad-x", "x": "!!"},
        {"kty": "OKP", "crv": "Ed25519", "kid": "short-x", "x": B64.encode([1u8; 31])},
        "not an object",
    ]});
    let keys = parse_jwks(&serde_json::to_vec(&body).unwrap(), "test").unwrap();
    assert_eq!(
        keys.kids().collect::<Vec<_>>(),
        ["alg-ed25519", "alg-eddsa", "foreign-d", "plain", "with-d"]
    );
    assert_eq!(keys.get("plain").unwrap().alg, None);
    assert_eq!(keys.get("alg-ed25519").unwrap().alg, Some(Alg::Ed25519));
    assert_eq!(keys.get("alg-eddsa").unwrap().alg, Some(Alg::EdDsa));
    // `d` は読まず、`x` だけを使う。
    assert_eq!(
        keys.get("foreign-d").unwrap().key,
        signing_key(8).verifying_key()
    );
}

/// 同じ kid が 2 本以上あれば、どちらの鍵か決められないので取り込まない。
#[test]
fn a_doubled_kid_is_dropped() {
    let keys = jwks(vec![
        jwk("dup", 1, None),
        jwk("ok", 2, None),
        jwk("dup", 3, None),
    ]);
    assert_eq!(keys.kids().collect::<Vec<_>>(), ["ok"]);
}

#[test]
fn a_jwks_without_a_keys_array_is_refused() {
    for body in [&b"not json"[..], b"{}", b"[]", br#"{"keys": {}}"#] {
        assert!(parse_jwks(body, "test").is_err(), "{body:?}");
    }
    assert!(parse_jwks(br#"{"keys": []}"#, "test").unwrap().is_empty());
}
