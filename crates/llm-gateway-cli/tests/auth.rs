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

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// `auth token` の試験の置き場: 手の鍵束 (kid `by-hand`) と、それを指す設定。
/// `[stats] dir` も試験の置き場に向ける (自動の鍵はその下)。
struct Face {
    dir: tempfile::TempDir,
    config: std::path::PathBuf,
}

impl Face {
    fn new(extra: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ring = dir.path().join("lab.jwks.jsonl");
        std::fs::write(&ring, stdout(&["auth", "keygen", "--kid", "by-hand"], "")).unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"
[stats]
dir = "{}"

[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["claude-opus-5"]

[ns.lab]
auth = "jwt"
keys_file = "{}"
max_ttl = "400d"
{extra}

[ns.open]
"#,
                dir.path().join("stats").display(),
                ring.display()
            ),
        )
        .unwrap();
        Self { dir, config }
    }

    fn config(&self) -> &str {
        self.config.to_str().unwrap()
    }

    fn auto_path(&self) -> std::path::PathBuf {
        self.dir.path().join("stats/jwt-keys/lab.jwks.jsonl")
    }

    /// daemon の入れ替えと同じ口で、自動の鍵を 1 本作る。
    fn rotate(&self) -> String {
        let config = llm_gateway::Config::load(&self.config).unwrap();
        let mut due = std::collections::HashMap::new();
        let jobs = llm_gateway::key_rotation::jobs(&config);
        llm_gateway::key_rotation::step(&jobs, &mut due, now(), &mut |b: &mut [u8]| {
            b.iter_mut().enumerate().for_each(|(i, x)| *x = i as u8 + 3)
        });
        jwt::auto::newest(&self.auto_path()).unwrap().unwrap().kid
    }

    fn token(&self, extra: &[&str]) -> std::process::Output {
        let mut args = vec!["auth", "token", "--config", self.config(), "--ns", "lab"];
        args.extend_from_slice(extra);
        run(&args, "")
    }

    /// 置き場の中身 (名前・長さ・mtime) の控え。書いていないことを比べる。
    fn snapshot(&self) -> Vec<(std::path::PathBuf, u64, std::time::SystemTime)> {
        fn walk(
            dir: &std::path::Path,
            out: &mut Vec<(std::path::PathBuf, u64, std::time::SystemTime)>,
        ) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    walk(&entry.path(), out);
                } else {
                    out.push((entry.path(), meta.len(), meta.modified().unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(self.dir.path(), &mut out);
        out.sort();
        out
    }
}

/// claims (署名部を除く payload) とヘッダを読む。
fn parts(token: &str) -> (serde_json::Value, serde_json::Value) {
    use base64::Engine as _;
    let decode = |part: &str| -> serde_json::Value {
        serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(part)
                .unwrap(),
        )
        .unwrap()
    };
    let mut it = token.trim().split('.');
    (decode(it.next().unwrap()), decode(it.next().unwrap()))
}

/// 最新の自動の鍵で署名した 1 行を出し、何も書かない。設定の iss / aud を載せ、
/// TTL の既定は 1 時間。検証は手の鍵束と自動の鍵束の両方を見る。
#[test]
fn token_signs_with_the_newest_automatic_key_and_writes_nothing() {
    let face = Face::new("iss = \"gw\"\naud = \"ns-lab\"");
    let kid = face.rotate();
    assert!(kid.starts_with("lab-"), "{kid}");
    let before = face.snapshot();
    let out = face.token(&["--sub", "helper-test", "--kid", "by-hand"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    let token = String::from_utf8(out.stdout).unwrap();
    assert_eq!(token.lines().count(), 1);
    assert_eq!(face.snapshot(), before, "issuing writes nothing");

    let (header, claims) = parts(&token);
    assert_eq!(
        header["kid"],
        kid.as_str(),
        "the automatic key wins over --kid"
    );
    assert_eq!(claims["sub"], "helper-test");
    assert_eq!(claims["iss"], "gw");
    assert_eq!(claims["aud"], "ns-lab");
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        3600
    );

    let config = llm_gateway::Config::load(&face.config).unwrap();
    let ns = config.namespace("lab").unwrap();
    let verified = ns.auth.verify("lab", Some(token.trim()));
    assert!(
        matches!(verified, llm_gateway::config::Authorization::Accepted(ref p) if p.kid.as_deref() == Some(kid.as_str())),
        "{verified:?}"
    );
}

/// 自動の鍵が無ければ `--kid` の手の鍵に落ちる。`--kid` も無ければ非 0 で、
/// stdout には何も出さない。どちらも鍵を作らない。
#[test]
fn token_falls_back_to_kid_or_refuses_without_an_automatic_key() {
    let face = Face::new("");
    let refused = face.token(&["--sub", "helper-test"]);
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(
        err.contains("no automatic key") && err.contains("--kid"),
        "{err}"
    );
    assert!(!face.auto_path().exists());
    assert!(
        !face.auto_path().parent().unwrap().exists(),
        "makes no directory either"
    );

    let out = face.token(&["--sub", "helper-test", "--kid", "by-hand"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let token = String::from_utf8(out.stdout).unwrap();
    assert_eq!(parts(&token).0["kid"], "by-hand");
    assert!(!face.auto_path().exists());

    // 自動の鍵のファイルが壊れていても `--kid` に落ちる。`--kid` が無ければ理由を言う。
    std::fs::create_dir_all(face.auto_path().parent().unwrap()).unwrap();
    std::fs::write(face.auto_path(), "{\"kty\":").unwrap();
    assert!(
        face.token(&["--sub", "s", "--kid", "by-hand"])
            .status
            .success()
    );
    let broken = face.token(&["--sub", "s"]);
    assert!(!broken.status.success() && broken.stdout.is_empty());
    assert!(String::from_utf8_lossy(&broken.stderr).contains("automatic keys"));
}

/// 自動の鍵の TTL は鍵に刻んだ上限 (周期と max_ttl の小さい方) まで、手の鍵は
/// max_ttl まで。超えたら非 0 で stdout は空。
#[test]
fn token_refuses_a_ttl_over_the_cap() {
    let face = Face::new("key_rotation = \"2d\"");
    face.rotate();
    for (ttl, ok) in [("2d", true), ("49h", false), ("400d", false)] {
        let out = face.token(&["--sub", "s", "--ttl", ttl]);
        assert_eq!(out.status.success(), ok, "{ttl}");
        if !ok {
            assert!(out.stdout.is_empty(), "{ttl}");
            assert!(String::from_utf8_lossy(&out.stderr).contains("2d"), "{ttl}");
        }
    }
    let manual = Face::new("");
    for (ttl, ok) in [("400d", true), ("401d", false)] {
        let out = manual.token(&["--sub", "s", "--kid", "by-hand", "--ttl", ttl]);
        assert_eq!(out.status.success(), ok, "{ttl}");
    }
}

/// 鍵を持たない ns・無い ns・検証を通らない sub は断る。
#[test]
fn token_needs_a_jwt_namespace_and_a_valid_subject() {
    let face = Face::new("");
    face.rotate();
    for (args, why) in [
        (
            vec![
                "auth",
                "token",
                "--config",
                face.config(),
                "--ns",
                "open",
                "--sub",
                "s",
            ],
            "does not use",
        ),
        (
            vec![
                "auth",
                "token",
                "--config",
                face.config(),
                "--ns",
                "nope",
                "--sub",
                "s",
            ],
            "lab, open",
        ),
        (
            vec!["auth", "token", "--config", face.config(), "--ns", "lab"],
            "--sub",
        ),
        (
            vec![
                "auth",
                "token",
                "--config",
                face.config(),
                "--ns",
                "lab",
                "--sub",
                "",
            ],
            "--sub",
        ),
    ] {
        let out = run(&args, "");
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(why), "{args:?}: {err}");
    }
}

/// 入れ替えた自動の鍵で `auth token` が出した token を、同じ設定を読んだ gateway が通す。
#[tokio::test]
async fn an_auth_token_from_a_rotated_key_opens_the_namespace() {
    let face = Face::new("");
    face.rotate();
    let token = String::from_utf8(face.token(&["--sub", "e2e"]).stdout).unwrap();

    let config = llm_gateway::Config::load(&face.config).unwrap();
    let store = llm_gateway::credential::file::FileStore::open(face.dir.path().join("credentials"))
        .unwrap();
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
    let status = reqwest::Client::new()
        .get(format!("http://{addr}/ns-lab/v1/models"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 200);
}
