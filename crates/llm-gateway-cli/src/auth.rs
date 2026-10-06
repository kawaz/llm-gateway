//! ns 認証 `jwt` の鍵と token を手元で作る (DR-0030 §6、DR-0037)。
//!
//! どれも標準出力に出すだけで、何も保存しない。鍵束 (`keys_file`、1 行 1 JWK) への
//! 追記は利用者が `>>` で行う。CLI が秘密をどこかに置くと、鍵束の窓口がファイルと
//! CLI の 2 つに割れる (DR-0030 §5)。
//!
//! `token` だけが設定を読み、gateway が入れ替える自動の鍵で発行する (DR-0037 §2)。
//! 発行は読むだけで、鍵のファイルを書くのは daemon の入れ替えだけ。

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use llm_gateway::config::{NsAuth, jwt};
use serde_json::{Value, json};

use crate::failure::Failure;
use crate::help;
use crate::options::{split, take_value};

pub fn dispatch(args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() || help::wanted(args) {
        print!("{}", help::AUTH);
        return Ok(ExitCode::SUCCESS);
    }
    let rest = &args[1..];
    let out = match args[0].as_str() {
        "token" => token(&parse(rest, &["config", "ns", "sub", "kid", "ttl"], &[])?)?,
        "keygen" => keygen(&parse(rest, &["kid"], &[])?)?,
        "jwks" => jwks(&parse(rest, &["key", "kid"], &[])?)?,
        "sign" => sign(&parse(
            rest,
            &["key", "kid", "sub", "ttl", "iss"],
            &["aud"],
        )?)?,
        other => {
            return Err(Failure::from(format!(
                "there is no `auth {other}` command. see `llm-gateway auth --help`"
            )));
        }
    };
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

/// 読み取った `--key value` の並び。`many` に挙げたものは何度でも書ける。
#[derive(Debug, Default)]
struct Parsed {
    one: std::collections::BTreeMap<String, String>,
    many: std::collections::BTreeMap<String, Vec<String>>,
}

impl Parsed {
    fn get(&self, key: &str) -> Option<&str> {
        self.one.get(key).map(String::as_str)
    }

    fn need(&self, key: &str) -> Result<&str, Failure> {
        self.get(key)
            .ok_or_else(|| Failure::from(format!("--{key} is required")))
    }
}

fn parse(args: &[String], one: &[&str], many: &[&str]) -> Result<Parsed, Failure> {
    let mut parsed = Parsed::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        // `--` から後ろは位置引数。`auth` の命令は位置引数を取らないので、何か
        // 続けば断る (オプションと取り違えたまま黙って進まない)。
        if arg == "--" {
            if let Some(extra) = it.next() {
                return Err(Failure::from(format!(
                    "`{extra}` is not expected; `auth` commands take options only"
                )));
            }
            break;
        }
        match split(arg) {
            Some((key, inline)) if one.contains(&key) => {
                let value = take_value(key, inline, &mut it)?;
                if parsed.one.insert(key.to_owned(), value).is_some() {
                    return Err(Failure::from(format!("--{key} is given twice")));
                }
            }
            Some((key, inline)) if many.contains(&key) => {
                let value = take_value(key, inline, &mut it)?;
                parsed.many.entry(key.to_owned()).or_default().push(value);
            }
            _ => return Err(Failure::from(format!("could not understand `{arg}`"))),
        }
    }
    Ok(parsed)
}

/// `auth token` の TTL の既定 (DR-0037 §2)。
///
/// Claude Code は JWT の `exp - 30s` で helper を呼び直すので期限切れの token は
/// 送られず、裏の呼び直しが失敗し続けても約 55 分は古い token で動く。
const DEFAULT_TOKEN_TTL_SECS: i64 = 3600;

/// 設定の ns の鍵で token を発行し、1 行で出す (DR-0037 §2・§14)。読むだけで、
/// 何も書かない。
///
/// 鍵は最新の自動の鍵。無ければ (読めなければ) `--kid` の手の鍵。TTL は鍵の上限
/// (自動の鍵は行に刻んだ `max_token_ttl`、手の鍵は無し) と `max_ttl` を超えられない。
fn token(parsed: &Parsed) -> Result<String, Failure> {
    let ns_name = parsed.need("ns")?;
    let subject = parsed.need("sub")?;
    if !jwt::subject_is_acceptable(subject) {
        return Err(Failure::from(format!(
            "--sub must be 1 to {} bytes without control characters",
            jwt::MAX_SUBJECT_LEN
        )));
    }
    let ttl = match parsed.get("ttl") {
        Some(raw) => llm_gateway::config::parse_duration_secs(raw)
            .map_err(|e| Failure::from(format!("--ttl: {e}")))?,
        None => DEFAULT_TOKEN_TTL_SECS,
    };
    let path = parsed
        .get("config")
        .map_or_else(llm_gateway::Config::default_path, PathBuf::from);
    let config = crate::load(&path)?;
    let Some(ns) = config.namespace(ns_name) else {
        return Err(Failure::from(format!(
            "{} has no namespace `{ns_name}`; namespaces: {}",
            path.display(),
            config.namespace_names().join(", ")
        )));
    };
    let NsAuth::Jwt(auth) = &ns.auth else {
        return Err(Failure::from(format!(
            "namespace `{ns_name}` does not use auth = \"jwt\"; there is no key to sign with"
        )));
    };
    let auto_path = config.auto_keys_path(ns_name);
    let automatic = match jwt::auto::newest(&auto_path) {
        Ok(found) => found,
        // 読めない自動の鍵は無いものとして `--kid` に落とす。`--kid` も無ければ理由を言う。
        Err(e) if parsed.get("kid").is_none() => {
            return Err(Failure::from(format!(
                "cannot read the automatic keys: {e}; fix or remove the broken line, or give --kid <kid> to sign with a key of keys_file"
            )));
        }
        Err(_) => None,
    };
    let (kid, key, cap) = match (automatic, parsed.get("kid")) {
        (Some(found), _) => (
            found.kid,
            found.key,
            found.mark.max_token_ttl.min(auth.max_ttl_secs),
        ),
        (None, Some(kid)) => {
            let ring_path = auth
                .ring
                .path()
                .ok_or_else(|| Failure::from("the namespace has no keys_file"))?;
            let text = std::fs::read_to_string(ring_path).map_err(|e| {
                Failure::from(format!("could not read {}: {e}", ring_path.display()))
            })?;
            let ring = jwt::read_key_ring(&text)
                .map_err(|e| Failure::from(format!("{}: {e}", ring_path.display())))?;
            let (kid, key) = pick(ring, Some(kid))?;
            (kid, key, auth.max_ttl_secs)
        }
        (None, None) => {
            return Err(Failure::from(format!(
                "namespace `{ns_name}` has no automatic key yet ({} does not hold one); start a unit that reads this configuration so it makes one, or give --kid <kid> to sign with a key of keys_file",
                auto_path.display()
            )));
        }
    };
    if ttl > cap {
        return Err(Failure::from(format!(
            "--ttl {} is longer than the key `{kid}` allows ({}); give a shorter --ttl",
            llm_gateway::config::format_duration_secs(ttl),
            llm_gateway::config::format_duration_secs(cap)
        )));
    }
    let now = llm_gateway::credential::time::now_unix();
    let mut claims = json!({"sub": subject, "iat": now, "exp": now + ttl});
    // ns が要求する iss / aud を載せ、そのまま検証を通る token にする。
    if let Some(iss) = &auth.iss {
        claims["iss"] = json!(iss);
    }
    if let Some(aud) = &auth.aud {
        claims["aud"] = json!(aud);
    }
    Ok(format!("{}\n", jwt::sign(&key, &kid, &claims)))
}

/// 新しい鍵ペアを作り、秘密鍵の JWK を 1 行で出す。
fn keygen(parsed: &Parsed) -> Result<String, Failure> {
    let kid = match parsed.get("kid") {
        Some(kid) => kid.to_owned(),
        None => {
            let mut tag = [0u8; 2];
            rand::fill(&mut tag);
            let today = llm_gateway::credential::time::local_date(
                llm_gateway::credential::time::now_unix(),
            );
            format!("{today}-{:02x}{:02x}", tag[0], tag[1])
        }
    };
    let mut seed = [0u8; 32];
    rand::fill(&mut seed);
    let key = jwt::signing_key(&seed);
    Ok(format!("{}\n", jwt::private_jwk(&kid, &key)))
}

/// 鍵束 (1 行 1 JWK) を読む。`-` (既定) は標準入力。
fn read_ring(parsed: &Parsed) -> Result<BTreeMap<String, jwt::SigningKey>, Failure> {
    let source = parsed.get("key").unwrap_or("-");
    let text = if source == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| Failure::from(format!("could not read the key ring from stdin: {e}")))?;
        text
    } else {
        std::fs::read_to_string(source)
            .map_err(|e| Failure::from(format!("could not read {source}: {e}")))?
    };
    jwt::read_key_ring(&text).map_err(|e| Failure::from(format!("{source}: {e}")))
}

/// `--kid` の行を選ぶ。省けば、1 行の鍵束ならその行。
fn pick(
    mut ring: BTreeMap<String, jwt::SigningKey>,
    kid: Option<&str>,
) -> Result<(String, jwt::SigningKey), Failure> {
    let listed = ring_kids(&ring);
    match kid {
        Some(kid) => match ring.remove(kid) {
            Some(key) => Ok((kid.to_owned(), key)),
            None => Err(Failure::from(format!(
                "the key ring has no kid `{kid}`; give one of: {}",
                listed
            ))),
        },
        None if ring.len() == 1 => Ok(ring.pop_first().expect("one key")),
        None => Err(Failure::from(format!(
            "the key ring has {} keys; give --kid (one of: {})",
            ring.len(),
            listed
        ))),
    }
}

fn ring_kids(ring: &BTreeMap<String, jwt::SigningKey>) -> String {
    ring.keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 鍵束の公開鍵だけを JWKS の JSON で出す。`--kid` があればその 1 本。
fn jwks(parsed: &Parsed) -> Result<String, Failure> {
    let ring = read_ring(parsed)?;
    let keys: Vec<Value> = match parsed.get("kid") {
        Some(kid) => {
            let (kid, key) = pick(ring, Some(kid))?;
            vec![jwt::public_jwk(&kid, &key.verifying_key())]
        }
        None => ring
            .iter()
            .map(|(kid, key)| jwt::public_jwk(kid, &key.verifying_key()))
            .collect(),
    };
    Ok(format!("{}\n", json!({ "keys": keys })))
}

/// 秘密鍵で JWT を鋳造し、1 行で出す。`iat = now`、`exp = now + ttl`。
fn sign(parsed: &Parsed) -> Result<String, Failure> {
    // 足りない引数は、標準入力を待つ前に言う。
    let subject = parsed.need("sub")?;
    let ttl = llm_gateway::config::parse_duration_secs(parsed.need("ttl")?)
        .map_err(|e| Failure::from(format!("--ttl: {e}")))?;
    let (kid, key) = pick(read_ring(parsed)?, parsed.get("kid"))?;
    let now = llm_gateway::credential::time::now_unix();
    let exp = now
        .checked_add(ttl)
        .ok_or_else(|| Failure::from("--ttl is too long to put an expiry on the token"))?;
    let mut claims = json!({"sub": subject, "iat": now, "exp": exp});
    if let Some(iss) = parsed.get("iss") {
        claims["iss"] = json!(iss);
    }
    match parsed.many.get("aud").map(Vec::as_slice) {
        None | Some([]) => {}
        Some([one]) => claims["aud"] = json!(one),
        Some(many) => claims["aud"] = json!(many),
    }
    Ok(format!("{}\n", jwt::sign(&key, &kid, &claims)))
}
