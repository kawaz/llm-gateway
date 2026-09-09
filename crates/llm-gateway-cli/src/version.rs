//! 何の版が置いてあり、何の版が走っているか (DR-0028 決定 9)。
//!
//! 版は 2 つある。**置いてある版** (`on_disk`) はディスクの実行ファイルに
//! `--version` を聞いたもので、次に上がるときの版。**走っている版**
//! (`running`) は動いているプロセス自身が答えたもので、今処理している版。
//!
//! 2 つが食い違うのは「入れ替えたのに上げ直していない」状態で、それを言える
//! のは両方を並べたときだけ。`--version` (1 行のテキスト) はこの CLI 自身の
//! 版しか言えないので、別の命令にしてある。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use llm_gateway::daemon::protocol::{self, Request, Which};
use llm_gateway::daemon::registry::Registry;
use serde_json::{Value, json};

use crate::failure::Failure;
use crate::service::platform;

pub fn run(args: &[String]) -> Result<ExitCode, Failure> {
    if let Some(unexpected) = args.first() {
        return Err(Failure::from(format!(
            "could not understand `{unexpected}`"
        )));
    }

    let registry = Registry::open();
    let units: Vec<(String, PathBuf)> = registry
        .list()?
        .into_iter()
        .map(|(name, unit)| (name, unit.binary_path))
        .collect();

    println!(
        "{}",
        report(
            env!("CARGO_PKG_VERSION"),
            registered_executable().as_deref(),
            asked().as_ref(),
            &units,
            &on_disk,
        )
    );
    Ok(ExitCode::SUCCESS)
}

/// 集めたものを 1 つの答えに組む。
///
/// ディスクを読む口 (`read`) を渡してもらうのは、試験が本物の実行ファイルを
/// 走らせずに済ませるため。
fn report(
    cli: &str,
    registered: Option<&Path>,
    answer: Option<&Value>,
    units: &[(String, PathBuf)],
    read: &dyn Fn(&Path) -> Option<String>,
) -> Value {
    let supervisor = registered.map(|exe| {
        let running = answer
            .and_then(|answer| answer.get("supervisor_version"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        pair(running, read(exe))
    });

    let units: Vec<Value> = units
        .iter()
        .map(|(name, binary)| {
            let running = running_version(answer, name);
            let mut row = json!({ "unit": name });
            let versions = pair(running, read(binary));
            if let (Some(row), Some(versions)) = (row.as_object_mut(), versions.as_object()) {
                row.extend(versions.clone());
            }
            row
        })
        .collect();

    json!({
        "cli": cli,
        // 登録されていなければ「監督者」という対象自体が無い。
        "supervisor": supervisor.unwrap_or(Value::Null),
        "units": units,
    })
}

/// 走っている版と置いてある版、そして上げ直しが要るか。
///
/// 要ると言えるのは**両方が分かったとき**だけ。片方が `null` なのは
/// 「食い違っていない」ではなく「比べられない」であって、そこで
/// `restart_needed: true` を出すと、答えない古い台を毎回上げ直させる。
fn pair(running: Option<String>, on_disk: Option<String>) -> Value {
    let needed = match (&running, &on_disk) {
        (Some(running), Some(on_disk)) => running != on_disk,
        _ => false,
    };
    json!({
        "running": running,
        "on_disk": on_disk,
        "restart_needed": needed,
    })
}

/// 監督者の答えから、1 台の走っている版を拾う。
fn running_version(answer: Option<&Value>, name: &str) -> Option<String> {
    answer?
        .get("units")?
        .as_array()?
        .iter()
        .find(|row| row.get("unit").and_then(Value::as_str) == Some(name))?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

/// 置いてある実行ファイルに、自分の版を聞く。
///
/// 走らせて聞くしかない。ファイルの中を読んでも、そこに書いてある版が
/// 何を指すのかはその binary の作りしだいで、こちらからは決められない。
fn on_disk(path: &Path) -> Option<String> {
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// `llm-gateway 0.43.7` から版だけを取る。
///
/// 名乗りが違うものは「分からない」にする。`--executable` で別の何かが
/// 焼かれていることもあり、その出力を版として読むと嘘になる。
fn parse(text: &str) -> Option<String> {
    let line = text.lines().next()?.trim();
    let version = line.strip_prefix("llm-gateway ")?.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

/// OS に登録されている実行ファイル。登録が無ければ `None`。
fn registered_executable() -> Option<PathBuf> {
    let kind = platform::kind();
    let path = crate::service::unit_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    platform::executable_of(kind, &text)
}

/// 監督者に、抱えている台の様子を聞く。居なければ `None`。
fn asked() -> Option<Value> {
    let socket = protocol::socket_path();
    if !socket.exists() {
        return None;
    }
    let runtime = tokio::runtime::Runtime::new().ok()?;
    let answer = runtime
        .block_on(async move { protocol::ask(&socket, &Request::Status(Which::all())).await })
        .ok()?;
    let value: Value = serde_json::from_str(&answer).ok()?;
    value.get("error").is_none().then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版を知っているふりをする口 (本物の実行ファイルを走らせない)。
    fn disk(pairs: &[(&'static str, &'static str)]) -> impl Fn(&Path) -> Option<String> + use<> {
        let pairs: Vec<(PathBuf, String)> = pairs
            .iter()
            .map(|(path, version)| (PathBuf::from(path), (*version).to_owned()))
            .collect();
        move |path| {
            pairs
                .iter()
                .find(|(known, _)| known == path)
                .map(|(_, version)| version.clone())
        }
    }

    fn answer(supervisor: &str, units: Value) -> Value {
        json!({"supervisor_version": supervisor, "units": units})
    }

    /// 走っている版と置いてある版が食い違えば、上げ直しが要ると言う。
    #[test]
    fn a_replaced_binary_that_is_still_running_the_old_one_says_so() {
        let value = report(
            "0.44.0",
            Some(Path::new("/bin/lg")),
            Some(&answer(
                "0.43.7",
                json!([{"unit": "stable", "version": "0.43.7"}]),
            )),
            &[("stable".to_owned(), PathBuf::from("/bin/unit"))],
            &disk(&[("/bin/lg", "0.44.0"), ("/bin/unit", "0.44.0")]),
        );

        assert_eq!(value["cli"], json!("0.44.0"));
        assert_eq!(value["supervisor"]["running"], json!("0.43.7"));
        assert_eq!(value["supervisor"]["on_disk"], json!("0.44.0"));
        assert_eq!(value["supervisor"]["restart_needed"], json!(true));
        assert_eq!(value["units"][0]["unit"], json!("stable"));
        assert_eq!(value["units"][0]["running"], json!("0.43.7"));
        assert_eq!(value["units"][0]["restart_needed"], json!(true));
    }

    /// 揃っているなら、上げ直しは要らない。
    #[test]
    fn nothing_needs_restarting_when_the_versions_match() {
        let value = report(
            "0.44.0",
            Some(Path::new("/bin/lg")),
            Some(&answer(
                "0.44.0",
                json!([{"unit": "a", "version": "0.44.0"}]),
            )),
            &[("a".to_owned(), PathBuf::from("/bin/lg"))],
            &disk(&[("/bin/lg", "0.44.0")]),
        );

        assert_eq!(value["supervisor"]["restart_needed"], json!(false));
        assert_eq!(value["units"][0]["restart_needed"], json!(false));
    }

    /// 監督者が居なければ、走っている版は誰にも聞けない。
    ///
    /// 聞けないことはエラーではない。置いてある版までは言える。
    #[test]
    fn without_a_supervisor_only_what_is_on_disk_is_known() {
        let value = report(
            "0.44.0",
            Some(Path::new("/bin/lg")),
            None,
            &[("a".to_owned(), PathBuf::from("/bin/lg"))],
            &disk(&[("/bin/lg", "0.44.0")]),
        );

        assert_eq!(value["supervisor"]["running"], Value::Null);
        assert_eq!(value["supervisor"]["on_disk"], json!("0.44.0"));
        // 比べられないので、上げ直しが要るとは言わない。
        assert_eq!(value["supervisor"]["restart_needed"], json!(false));
        assert_eq!(value["units"][0]["running"], Value::Null);
        assert_eq!(value["units"][0]["restart_needed"], json!(false));
    }

    /// OS に登録していない使い方もある。その時に「監督者」は無い。
    #[test]
    fn an_unregistered_supervisor_is_null() {
        let value = report("0.44.0", None, None, &[], &disk(&[]));
        assert_eq!(value["supervisor"], Value::Null);
        assert_eq!(value["units"], json!([]));
    }

    /// 置いてある実行ファイルが答えない (消えた / 別物) ことはある。
    #[test]
    fn a_binary_that_cannot_be_asked_is_null() {
        let value = report(
            "0.44.0",
            Some(Path::new("/bin/gone")),
            Some(&answer("0.44.0", json!([]))),
            &[],
            &disk(&[]),
        );

        assert_eq!(value["supervisor"]["on_disk"], Value::Null);
        assert_eq!(value["supervisor"]["running"], json!("0.44.0"));
        assert_eq!(value["supervisor"]["restart_needed"], json!(false));
    }

    /// 名乗りが揃っているものだけを版として読む。
    #[test]
    fn only_our_own_version_line_is_read() {
        assert_eq!(parse("llm-gateway 0.43.7\n").as_deref(), Some("0.43.7"));
        assert_eq!(parse("something else 1.0\n"), None);
        assert_eq!(parse("llm-gateway \n"), None);
        assert_eq!(parse(""), None);
    }
}
