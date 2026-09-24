//! ns 認証 `jwt` の鍵と token を手元で作る (DR-0030 §6)。
//!
//! どれも標準出力に出すだけで、何も保存しない。秘密鍵をどこに置くか
//! (1Password / 権限を絞ったファイル) は使う人が決める。gateway が持つ秘密は
//! `issued` 用の署名鍵だけ、という線 (DR-0030 §5) をここで崩さない。

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
        "jwks" => jwks(&parse(rest, &["key", "kid", "ns", "format"], &[])?)?,
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

/// 秘密鍵を読む。`-` (既定) は標準入力。`--kid` があれば JWK の kid より優先する。
fn read_key(parsed: &Parsed) -> Result<(String, jwt::SigningKey), Failure> {
    let source = parsed.get("key").unwrap_or("-");
    let text = if source == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| Failure::from(format!("could not read the key from stdin: {e}")))?;
        text
    } else {
        std::fs::read_to_string(source)
            .map_err(|e| Failure::from(format!("could not read {source}: {e}")))?
    };
    let (kid, key) = jwt::read_private_jwk(&text).map_err(Failure::from)?;
    let kid = parsed
        .get("kid")
        .map(str::to_owned)
        .or(kid)
        .ok_or_else(|| Failure::from("the key has no kid; give --kid"))?;
    Ok((kid, key))
}

/// 設定に貼る公開鍵 (TOML の断片) か、JWKS の JSON を出す。
fn jwks(parsed: &Parsed) -> Result<String, Failure> {
    let (kid, key) = read_key(parsed)?;
    let public = key.verifying_key();
    match parsed.get("format").unwrap_or("toml") {
        "toml" => {
            let ns = parsed.get("ns").unwrap_or("<name>");
            Ok(format!(
                "[ns.{ns}.keys.{}]\nalg = \"EdDSA\"\npublic = \"{}\"\n",
                toml_key(&kid),
                jwt::public_key_text(&public)
            ))
        }
        "jwks" => Ok(format!(
            "{}\n",
            json!({"keys": [jwt::public_jwk(&kid, &public)]})
        )),
        other => Err(Failure::from(format!(
            "--format is `toml` or `jwks`, not `{other}`"
        ))),
    }
}

/// TOML の鍵として書ける形 (素の鍵で書けなければ引用する)。
fn toml_key(kid: &str) -> String {
    if !kid.is_empty()
        && kid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        kid.to_owned()
    } else {
        Value::String(kid.to_owned()).to_string()
    }
}

/// 秘密鍵で JWT を鋳造し、1 行で出す。`iat = now`、`exp = now + ttl`。
fn sign(parsed: &Parsed) -> Result<String, Failure> {
    // 足りない引数は、標準入力を待つ前に言う。
    let subject = parsed.need("sub")?;
    let ttl = llm_gateway::config::parse_duration_secs(parsed.need("ttl")?)
        .map_err(|e| Failure::from(format!("--ttl: {e}")))?;
    let (kid, key) = read_key(parsed)?;
    let now = llm_gateway::credential::time::now_unix();
    let mut claims = json!({"sub": subject, "iat": now, "exp": now + ttl});
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
