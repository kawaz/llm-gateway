//! ブラウザで認可を通し、認証情報を保存する。
//!
//! 設定を指すのは、宣言された種別と置き場を見るため。宛先の解決 (DR-0028
//! 決定 6) とは別の話なので、`--config` はここに残る。

use std::path::PathBuf;
use std::process::ExitCode;

use llm_gateway::Config;
use llm_gateway::config::CredentialSpec;
use llm_gateway::credential::file::FileStore;
use llm_gateway::credential::{CredentialId, Kind, Persistence, StoredCredential, oauth};

use crate::failure::Failure;
use crate::help;
use crate::load;
use crate::options::take_value;

/// `login` に渡された内容。
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    name: String,
    kind: Kind,
    config_path: PathBuf,
}

pub fn run(args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() || help::wanted(args) {
        print!("{}", help::TOP);
        return Ok(ExitCode::SUCCESS);
    }
    let Args {
        name,
        kind,
        config_path,
    } = parse(args)?;
    let config = load(&config_path)?;
    let declared = config.credentials.get(&name);
    check_declared_type(&name, kind, declared)?;

    let dir = config.store.resolve_dir();
    let id = CredentialId::new(&name);

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))?;

    runtime.block_on(async move {
        let store = FileStore::open(&dir).map_err(|e| Failure::from(e.to_string()))?;

        // 受け口を開いてから URL を出す。出してから開くまでの間に戻ってこられても
        // 取りこぼさない。
        let authorization = oauth::begin(kind)
            .await
            .map_err(|e| Failure::from(e.to_string()))?;

        println!("open the following URL in a browser and approve the request:");
        println!();
        println!("  {}", authorization.url());
        println!();
        println!("after you approve, the token is fetched, checked, and then saved.");
        println!("the browser page waits until that result is ready.");
        open_browser(authorization.url());

        // 交換・確認・保存が全部済んでから、ブラウザにも端末にも結果が出る。
        let credential = authorization
            .finish(|tokens| save(&store, &id, kind, tokens))
            .await
            .map_err(|e| Failure::from(e.to_string()))?;

        println!(
            "checked that the token works, and saved it to {}",
            dir.join(format!("{name}.json")).display()
        );
        if !credential.payload.email().is_empty() {
            println!("account {}", credential.payload.email());
        }
        if declared.is_none() {
            print_config_hint(&name, kind);
        }
        Ok(ExitCode::SUCCESS)
    })
}

/// 認可で得た token を置き場に書く。
///
/// 土台を読んでから書くまでを締め出す (DR-0010)。再ログインするのは refresh
/// token が失効したときで、その裏では常駐している側が同じ認証情報の更新を
/// 試している。締め出さないと、認可の結果と相手の書き込みが互いを消し合う。
fn save<P: Persistence>(
    store: &P,
    id: &CredentialId,
    kind: Kind,
    tokens: &oauth::Tokens,
) -> llm_gateway::Result<StoredCredential> {
    let _guard = store.lock(id)?;
    llm_gateway::credential::save_login(store, id, kind, tokens)
}

fn parse(args: &[String]) -> Result<Args, Failure> {
    let mut name: Option<String> = None;
    let mut type_name: Option<String> = None;
    let mut config_path: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let Some(rest) = arg.strip_prefix("--") else {
            if let Some(previous) = name.replace(arg.clone()) {
                return Err(Failure::from(format!(
                    "two names were given (`{previous}` and `{arg}`). only one name is allowed"
                )));
            }
            continue;
        };
        let (key, inline) = match rest.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (rest, None),
        };
        match key {
            "type" => type_name = Some(take_value(key, inline, &mut it)?),
            "config" => config_path = Some(PathBuf::from(take_value(key, inline, &mut it)?)),
            other => return Err(Failure::from(format!("could not understand `--{other}`"))),
        }
    }

    let name = name.ok_or_else(|| {
        Failure::from(
            "no credential name was given. \
give it as `llm-gateway login --type claude_oauth <name>`",
        )
    })?;
    check_name(&name)?;

    let type_name = type_name
        .ok_or_else(|| Failure::from("--type is missing. give claude_oauth or codex_oauth"))?;
    let kind = Kind::from_config_type(&type_name).ok_or_else(|| {
        Failure::from(format!(
            "cannot log in to `{type_name}`. give claude_oauth or codex_oauth"
        ))
    })?;

    Ok(Args {
        name,
        kind,
        config_path: config_path.unwrap_or_else(Config::default_path),
    })
}

/// 名前はそのままファイル名になる。置き場の外に書けてしまう形を弾く。
///
/// `-` 始まりも弾く。綴りを間違えたオプションが名前として通ると、意図しない
/// ファイルに保存される。
fn check_name(name: &str) -> Result<(), Failure> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.starts_with('-')
    {
        return Err(Failure::from(format!(
            "`{name}` cannot be used as a credential name. \
the name becomes the file name <name>.json as it is"
        )));
    }
    Ok(())
}

/// 設定に宣言があるなら、種別が食い違っていないか見る。
///
/// 取り違えたまま保存すると、gateway が別の作法で使おうとして 401 になる。
/// 原因が認証側にあると気づきにくいので、保存する前に止める。
fn check_declared_type(
    name: &str,
    kind: Kind,
    declared: Option<&CredentialSpec>,
) -> Result<(), Failure> {
    // 宣言が無くても保存はする。先に認証情報を取ってから config.toml を書く
    // 順でも困らないようにしておく (書き方は保存後に案内する)。
    let Some(spec) = declared else {
        return Ok(());
    };

    match login_kind_of(spec) {
        Some(declared_kind) if declared_kind == kind => Ok(()),
        Some(declared_kind) => Err(Failure::from(format!(
            "`{name}` is declared as type = \"{t}\" in config.toml. \
give --type {t}, or use another name",
            t = declared_kind.config_type()
        ))),
        None => Err(Failure::from(format!(
            "`{name}` is declared in config.toml as a type that needs no login. \
only claude_oauth and codex_oauth are used through login"
        ))),
    }
}

/// 設定の宣言に対応する login の種別。login できない種別は `None`。
fn login_kind_of(spec: &CredentialSpec) -> Option<Kind> {
    match spec {
        CredentialSpec::ClaudeOauth => Some(Kind::Claude),
        CredentialSpec::CodexOauth => Some(Kind::Codex),
        CredentialSpec::BedrockApiKey => None,
    }
}

/// 保存はできたが設定に宣言が無い状態。次に何を書けばよいか示す。
fn print_config_hint(name: &str, kind: Kind) {
    println!();
    println!("`{name}` is not in [credentials] of config.toml. add the following to use it:");
    println!();
    println!("  [credentials.{name}]");
    println!("  type = \"{}\"", kind.config_type());
}

/// ブラウザを開く。開けなくても止めない (URL は既に出してある)。
fn open_browser(url: &str) {
    // `--` は URL が `-` で始まってもオプションとして読まれないため。
    let outcome = std::process::Command::new("open")
        .arg("--")
        .arg(url)
        .status();
    let reason = match outcome {
        Ok(status) if status.success() => return,
        Ok(status) => format!("open exited with {status}"),
        Err(e) => e.to_string(),
    };
    eprintln!("could not open a browser ({reason}). open the URL above yourself");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// 置き場に何をした順に、何をしたかを覚える偽物。
    #[derive(Default)]
    struct Recorder {
        steps: Arc<Mutex<Vec<&'static str>>>,
        current: Mutex<Option<StoredCredential>>,
    }

    /// 手放したことも記録に残す。締め出しっぱなしを見つけるため。
    struct Mark(Arc<Mutex<Vec<&'static str>>>);

    impl Drop for Mark {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("unlock");
        }
    }

    impl Recorder {
        fn holding(c: StoredCredential) -> Self {
            Self {
                steps: Arc::default(),
                current: Mutex::new(Some(c)),
            }
        }

        fn note(&self, step: &'static str) {
            self.steps.lock().unwrap().push(step);
        }

        fn steps(&self) -> Vec<&'static str> {
            self.steps.lock().unwrap().clone()
        }
    }

    impl Persistence for Recorder {
        type Guard = Mark;

        fn load(&self, _id: &CredentialId) -> llm_gateway::Result<StoredCredential> {
            self.note("load");
            self.current
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| llm_gateway::Error::Credential {
                    id: "c".to_owned(),
                    reason: "not stored yet".to_owned(),
                })
        }
        fn store(&self, _id: &CredentialId, v: &StoredCredential) -> llm_gateway::Result<()> {
            self.note("store");
            *self.current.lock().unwrap() = Some(v.clone());
            Ok(())
        }
        fn list(&self) -> llm_gateway::Result<Vec<CredentialId>> {
            Ok(vec![])
        }
        fn lock(&self, _id: &CredentialId) -> llm_gateway::Result<Self::Guard> {
            self.note("lock");
            Ok(Mark(Arc::clone(&self.steps)))
        }
        fn version(&self, _id: &CredentialId) -> Option<u64> {
            None
        }
    }

    fn fresh_tokens() -> oauth::Tokens {
        oauth::Tokens {
            access_token: "at-new".into(),
            refresh_token: "rt-new".into(),
            id_token: None,
            expires_in: 28_800,
            email: Some("someone@example.com".into()),
            account_id: None,
        }
    }

    /// 認可の結果を書くまでの間、置き場を締め出す。
    ///
    /// 再ログインは refresh token が失効したときに走るので、常駐している側が
    /// 同じ認証情報の更新を試している最中に当たりやすい。
    #[test]
    fn a_login_writes_under_the_lock() {
        let disk = Recorder::default();
        let id = CredentialId::new("c");

        save(&disk, &id, Kind::Claude, &fresh_tokens()).unwrap();

        assert_eq!(
            disk.steps(),
            vec!["lock", "load", "store", "unlock"],
            "blocked before reading the base, released after writing"
        );
    }

    /// 締め出している間に読み直すので、運用側の値を土台のまま引き継げる。
    #[test]
    fn a_login_keeps_what_the_operator_set() {
        let mut existing = StoredCredential::new(llm_gateway::credential::Payload::ClaudeOauth(
            llm_gateway::credential::OauthTokens {
                access_token: "at-old".into(),
                refresh_token: "rt-old".into(),
                expired: "2026-07-28T02:54:00+09:00".into(),
                email: "someone@example.com".into(),
                extra: Default::default(),
            },
        ));
        existing.priority = 10;
        existing.excluded_models = vec!["claude-opus-*".to_owned()];

        let disk = Recorder::holding(existing);
        let saved = save(
            &disk,
            &CredentialId::new("c"),
            Kind::Claude,
            &fresh_tokens(),
        )
        .unwrap();

        assert_eq!(saved.payload.secret(), "at-new");
        assert_eq!(saved.priority, 10);
        assert_eq!(saved.excluded_models, vec!["claude-opus-*"]);
    }

    fn parse_login(list: &[&str]) -> Result<Args, Failure> {
        parse(&args(list))
    }

    #[test]
    fn login_takes_a_type_and_a_name() {
        let got = parse_login(&["--type", "claude_oauth", "claude-main"]).unwrap();
        assert_eq!(got.name, "claude-main");
        assert_eq!(got.kind, Kind::Claude);
        assert_eq!(got.config_path, Config::default_path());
    }

    /// オプションはメイン引数の後ろにも置ける。`=` 付きでも書ける。
    #[test]
    fn login_options_may_follow_the_name() {
        let separate = parse_login(&["codex-main", "--type", "codex_oauth"]).unwrap();
        let inline = parse_login(&["--type=codex_oauth", "codex-main"]).unwrap();

        assert_eq!(separate, inline);
        assert_eq!(separate.kind, Kind::Codex);
    }

    #[test]
    fn login_takes_a_config_path() {
        let got = parse_login(&["--type", "claude_oauth", "n", "--config", "/tmp/c.toml"]).unwrap();
        assert_eq!(got.config_path, PathBuf::from("/tmp/c.toml"));

        let inline = parse_login(&["--type", "claude_oauth", "n", "--config=/tmp/c.toml"]).unwrap();
        assert_eq!(inline.config_path, PathBuf::from("/tmp/c.toml"));
    }

    /// 何が足りないかを言う (help を読み直させない)。
    #[test]
    fn login_says_what_is_missing() {
        let e = parse_login(&["--type", "claude_oauth"]).unwrap_err();
        assert!(e.message().contains("name"), "{e:?}");

        let e = parse_login(&["claude-main"]).unwrap_err();
        assert!(e.message().contains("--type"), "{e:?}");

        let e = parse_login(&["--type"]).unwrap_err();
        assert!(e.message().contains("--type needs a value"), "{e:?}");
    }

    /// login できない種別は、指定できる語を添えて断る。
    #[test]
    fn login_rejects_types_without_an_authorization_flow() {
        for bad in ["bedrock_api_key", "relay", "claude", "oauth"] {
            let e = parse_login(&["--type", bad, "n"]).unwrap_err();
            assert!(e.message().contains("claude_oauth"), "{bad} → {e:?}");
        }
    }

    #[test]
    fn login_rejects_unknown_options() {
        let e = parse_login(&["--type", "claude_oauth", "n", "--nope"]).unwrap_err();
        assert!(e.message().contains("--nope"), "{e:?}");
    }

    /// 名前が 2 つあると、どちらに保存されるか分からない。黙って選ばない。
    #[test]
    fn login_rejects_two_names() {
        let e = parse_login(&["--type", "claude_oauth", "a", "b"]).unwrap_err();
        assert!(e.message().contains("only one name"), "{e:?}");
        assert!(
            e.message().contains("`a`") && e.message().contains("`b`"),
            "shows both: {e:?}"
        );
    }

    /// 名前はそのままファイル名になる。置き場の外に書ける形を通さない。
    /// 綴りを誤ったオプション (`-type`) も名前として受け取らない。
    #[test]
    fn login_rejects_names_that_escape_the_store() {
        for bad in ["", ".", "..", "../../etc/passwd", "sub/name", "-type"] {
            let e = parse_login(&["--type", "claude_oauth", bad]).unwrap_err();
            assert!(
                e.message().contains("cannot be used") || e.message().contains("name"),
                "{bad} → {e:?}"
            );
        }
    }

    fn claude_oauth() -> CredentialSpec {
        CredentialSpec::ClaudeOauth
    }

    fn without_login() -> CredentialSpec {
        CredentialSpec::BedrockApiKey
    }

    /// 宣言が無い名前でも保存はできる (設定を書く前に取れる)。
    #[test]
    fn undeclared_name_is_allowed() {
        assert!(check_declared_type("new", Kind::Claude, None).is_ok());
    }

    #[test]
    fn declared_type_may_match() {
        assert!(check_declared_type("c", Kind::Claude, Some(&claude_oauth())).is_ok());
    }

    /// 種別を取り違えたまま保存すると、使う段になって 401 になる。手前で止める。
    #[test]
    fn declared_type_mismatch_is_refused() {
        let e = check_declared_type("c", Kind::Codex, Some(&claude_oauth())).unwrap_err();
        assert!(e.message().contains("claude_oauth"), "{e:?}");
    }

    #[test]
    fn declared_type_without_login_is_refused() {
        let e = check_declared_type("r", Kind::Claude, Some(&without_login())).unwrap_err();
        assert!(e.message().contains("login"), "{e:?}");
    }
}
