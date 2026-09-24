//! `auth keygen` / `jwks` / `sign` を実際のコマンドとして走らせ、出力が gateway の
//! 検証と同じ規約で合っていることを確かめる。

use std::io::Write as _;
use std::process::{Command, Stdio};

use gateway_core::ns::jwt;

fn run(args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_llm-gateway"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(args: &[&str], stdin: &str) -> String {
    let out = run(args, stdin);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// keygen の秘密鍵を jwks に食わせると、同じ公開鍵が設定の形と JWKS の形で出る。
#[test]
fn jwks_prints_the_public_half_of_keygen() {
    let private = stdout(&["auth", "keygen", "--kid", "mbp-2026-09"], "");
    let jwk: serde_json::Value = serde_json::from_str(&private).unwrap();
    let x = jwk["x"].as_str().unwrap();

    let toml_text = stdout(&["auth", "jwks", "--ns", "claude"], &private);
    assert!(
        toml_text.starts_with("[ns.claude.keys.mbp-2026-09]\n"),
        "{toml_text}"
    );
    let table: toml::Table = toml::from_str(&toml_text).unwrap();
    let key = &table["ns"]["claude"]["keys"]["mbp-2026-09"];
    assert_eq!(key["alg"].as_str(), Some("EdDSA"));
    assert_eq!(key["public"].as_str(), Some(x));

    let set: serde_json::Value =
        serde_json::from_str(&stdout(&["auth", "jwks", "--format", "jwks"], &private)).unwrap();
    assert_eq!(set["keys"][0]["x"].as_str(), Some(x));
    assert_eq!(set["keys"][0]["kid"].as_str(), Some("mbp-2026-09"));
    assert!(
        set["keys"][0].get("d").is_none(),
        "the private part never leaves"
    );
}

/// sign で鋳造した token は、jwks の公開鍵を載せた検証を通る。
#[test]
fn a_signed_token_passes_the_gateway_check() {
    let private = stdout(&["auth", "keygen"], "");
    let jwk: serde_json::Value = serde_json::from_str(&private).unwrap();
    let kid = jwk["kid"].as_str().unwrap().to_owned();
    let token = stdout(
        &[
            "auth",
            "sign",
            "--key",
            "-",
            "--sub",
            "kawaz-mbp",
            "--ttl",
            "180d",
            "--iss",
            "cli",
            "--aud",
            "ns-claude",
        ],
        &private,
    );
    let auth = jwt::JwtAuth {
        keys: [(
            kid.clone(),
            jwt::parse_public_key(jwk["x"].as_str().unwrap()).unwrap(),
        )]
        .into(),
        max_ttl_secs: 180 * 86_400,
        iss: Some("cli".into()),
        aud: Some("ns-claude".into()),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let verified = auth.verify(token.trim(), now).unwrap();
    assert_eq!(verified.subject, "kawaz-mbp");
    assert_eq!(verified.kid, kid);
}

/// 長すぎる `--ttl` は、期限が桁あふれする前に引数の誤りとして断る。
#[test]
fn an_overlong_ttl_is_refused() {
    let private = stdout(&["auth", "keygen"], "");
    for ttl in ["9223372036854775807s", "106751991167301d"] {
        let out = run(&["auth", "sign", "--sub", "x", "--ttl", ttl], &private);
        assert!(!out.status.success(), "{ttl}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("ttl"),
            "{ttl}"
        );
    }
}

/// `--` はオプションの終わり。`auth` は位置引数を取らないので、続きがあれば断る。
#[test]
fn a_double_dash_ends_the_options() {
    let private = stdout(&["auth", "keygen"], "");
    assert!(run(&["auth", "jwks", "--"], &private).status.success());
    let out = run(&["auth", "jwks", "--", "extra"], &private);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("extra"));
}

/// 足りない引数は、標準入力を待たずに言う。help は引数なしでも出る。
#[test]
fn missing_arguments_are_named() {
    let out = run(&["auth", "sign", "--sub", "x"], "");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--ttl"));
    assert!(stdout(&["auth"], "").contains("keygen"));
    let bad = run(&["auth", "jwks"], "{\"kty\":\"RSA\"}");
    assert!(!bad.status.success());
}

/// `auth keygen` → `auth jwks` → 設定 → `auth sign` の出力をそのまま `Bearer` に載せると、
/// 立てた gateway の jwt の ns が通す。
#[tokio::test]
async fn a_cli_minted_token_opens_a_jwt_namespace() {
    let private = stdout(&["auth", "keygen", "--kid", "e2e-key"], "");
    let keys = stdout(&["auth", "jwks", "--ns", "claude"], &private);
    let token = stdout(&["auth", "sign", "--sub", "e2e", "--ttl", "1h"], &private);

    let state = tempfile::tempdir().unwrap();
    let config: llm_gateway::Config = toml::from_str(&format!(
        r#"
[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["claude-opus-5"]

[ns.claude]
auth = "jwt"
max_ttl = "1d"

{keys}"#
    ))
    .unwrap();
    config.validate().unwrap();
    let store =
        llm_gateway::credential::file::FileStore::open(state.path().join("credentials")).unwrap();
    let gateway = std::sync::Arc::new(llm_gateway::Gateway::new(&config, store).unwrap());
    gateway.refresh_models().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = llm_gateway_server::router(gateway);
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    let client = reqwest::Client::new();
    let models = client
        .get(format!("http://{addr}/ns-claude/v1/models"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    let without = client
        .get(format!("http://{addr}/ns-claude/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(without.status(), 401);
}
