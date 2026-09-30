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
    // 引数の検査で先に終わる子は stdin を読まずに閉じるので、書き込みの EPIPE は
    // 失敗ではない (Linux では pipe が即座に閉じる)。
    match child.stdin.take().unwrap().write_all(stdin.as_bytes()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(e) => panic!("writing the child's stdin: {e}"),
    }
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

/// `keygen >> ring` を 2 回した鍵束を書く。
fn two_key_ring(dir: &std::path::Path) -> std::path::PathBuf {
    let ring = dir.join("claude.jwks.jsonl");
    let mut text = stdout(&["auth", "keygen", "--kid", "mbp"], "");
    text.push_str(&stdout(&["auth", "keygen", "--kid", "mini"], ""));
    std::fs::write(&ring, text).unwrap();
    ring
}

/// jwks は鍵束の全行の公開鍵を出し、秘密の部分 (`d`) は出さない。`--kid` で 1 本に絞れる。
#[test]
fn jwks_prints_the_public_half_of_the_key_ring() {
    let dir = tempfile::tempdir().unwrap();
    let ring = two_key_ring(dir.path());
    let key = ring.to_str().unwrap();
    let text = std::fs::read_to_string(&ring).unwrap();
    let private: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let set: serde_json::Value =
        serde_json::from_str(&stdout(&["auth", "jwks", "--key", key], "")).unwrap();
    let keys = set["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2);
    for (public, private) in keys.iter().zip(&private) {
        assert_eq!(public["kid"], private["kid"]);
        assert_eq!(public["x"], private["x"]);
        assert!(public.get("d").is_none(), "the private part never leaves");
    }
    assert!(
        !stdout(&["auth", "jwks", "--key", key], "").contains(private[0]["d"].as_str().unwrap())
    );

    let one: serde_json::Value =
        serde_json::from_str(&stdout(&["auth", "jwks", "--kid", "mini"], &text)).unwrap();
    assert_eq!(one["keys"].as_array().unwrap().len(), 1);
    assert_eq!(one["keys"][0]["kid"].as_str(), Some("mini"));
    assert!(one["keys"][0].get("d").is_none());

    let missing = run(&["auth", "jwks", "--key", key, "--kid", "nope"], "");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("mbp, mini"));
}

/// `keygen >> ring` を 2 回 → `sign --kid` で鋳造した token は、同じ鍵束を読んだ
/// gateway の検証を通り、kid はその行のもの。
#[test]
fn a_signed_token_passes_the_gateway_check() {
    let dir = tempfile::tempdir().unwrap();
    let ring = two_key_ring(dir.path());
    let token = stdout(
        &[
            "auth",
            "sign",
            "--key",
            ring.to_str().unwrap(),
            "--kid",
            "mini",
            "--sub",
            "kawaz-mbp",
            "--ttl",
            "180d",
            "--iss",
            "cli",
            "--aud",
            "ns-claude",
        ],
        "",
    );
    let auth = jwt::JwtAuth::from_file(
        ring,
        180 * 86_400,
        Some("cli".into()),
        Some("ns-claude".into()),
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let verified = auth.verify(token.trim(), now).unwrap();
    assert_eq!(verified.subject, "kawaz-mbp");
    assert_eq!(verified.kid, "mini");
}

/// `--kid` を省けるのは 1 行の鍵束だけ。複数行なら候補を挙げて断る。
#[test]
fn sign_without_kid_needs_a_one_line_ring() {
    let one = stdout(&["auth", "keygen", "--kid", "only"], "");
    assert!(
        run(&["auth", "sign", "--sub", "x", "--ttl", "1h"], &one)
            .status
            .success()
    );

    let dir = tempfile::tempdir().unwrap();
    let ring = two_key_ring(dir.path());
    let out = run(
        &[
            "auth",
            "sign",
            "--key",
            ring.to_str().unwrap(),
            "--sub",
            "x",
            "--ttl",
            "1h",
        ],
        "",
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("2 keys") && err.contains("--kid") && err.contains("mbp, mini"),
        "{err}"
    );
    let out = run(
        &[
            "auth",
            "sign",
            "--key",
            ring.to_str().unwrap(),
            "--kid",
            "nope",
            "--sub",
            "x",
            "--ttl",
            "1h",
        ],
        "",
    );
    assert!(!out.status.success());
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

/// `auth keygen` → 鍵束 → 設定の `keys_file` → `auth sign` の出力をそのまま `Bearer` に載せると、
/// 立てた gateway の jwt の ns が通す。
#[tokio::test]
async fn a_cli_minted_token_opens_a_jwt_namespace() {
    let private = stdout(&["auth", "keygen", "--kid", "e2e-key"], "");
    let token = stdout(&["auth", "sign", "--sub", "e2e", "--ttl", "1h"], &private);

    let state = tempfile::tempdir().unwrap();
    let ring = state.path().join("claude.jwks.jsonl");
    std::fs::write(&ring, &private).unwrap();
    let config: llm_gateway::Config = toml::from_str(&format!(
        r#"
[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["claude-opus-5"]

[ns.claude]
auth = "jwt"
max_ttl = "1d"
keys_file = "{}"
"#,
        ring.display()
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
