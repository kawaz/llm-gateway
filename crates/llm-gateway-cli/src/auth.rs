//! ns 認証 `jwt` の鍵と token を手元で作る (DR-0030 §6)。
//!
//! どれも標準出力に出すだけで、何も保存しない。鍵束 (`keys_file`、1 行 1 JWK) への
//! 追記は利用者が `>>` で行う。CLI が秘密をどこかに置くと、鍵束の窓口がファイルと
//! CLI の 2 つに割れる (DR-0030 §5)。

use std::collections::BTreeMap;
use std::io::Read as _;
use std::process::ExitCode;

use llm_gateway::config::jwt;
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
