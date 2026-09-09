//! OS ごとの登録の形 (DR-0028 決定 7)。
//!
//! 登録するのは監督者 1 つだけなので、書くファイルも 1 つ、叩く命令も
//! 数えるほどしかない。ここでは「何を書いて、何を叩くか」を組み立てるだけで、
//! 実際に書いたり叩いたりはしない — そうしておくと `--dry-run` が同じ物を
//! 出せるし、試験も本物の launchctl を呼ばずに済む。

use std::path::PathBuf;

use serde_json::{Value, json};

/// どの OS の作法で登録するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// macOS の launchd (user domain)。
    Launchd,
    /// Linux の systemd user unit。手元に実機が無く、未検証 (DR-0028 未確定)。
    Systemd,
}

/// 登録先の名前。既存の `com.kawaz.llm-gateway-{stable,unstable}` と衝突させない。
pub const LABEL: &str = "jp.kawaz.llm-gateway.supervise";

/// systemd 側の unit 名。
pub const UNIT_NAME: &str = "llm-gateway-supervise.service";

/// 叩く命令 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub program: String,
    pub args: Vec<String>,
}

impl Step {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    /// `--dry-run` で見せる形。人が読んでそのまま打てる並びにする。
    pub fn to_json(&self) -> Value {
        let mut line = vec![json!(self.program)];
        line.extend(self.args.iter().map(|a| json!(a)));
        json!(line)
    }
}

/// 登録に必要な材料をどこから取るか。
///
/// 実環境からも試験の一時ディレクトリからも同じ形で渡せるようにしておく。
#[derive(Debug, Clone)]
pub struct Env {
    /// `daemon supervise` を走らせる実行ファイル (絶対パス)。
    pub exe: PathBuf,
    /// unit ファイルを置くディレクトリ (`~/Library/LaunchAgents` など)。
    pub unit_dir: PathBuf,
    /// 監督者の stdout / stderr を流す先。
    pub log: PathBuf,
    /// launchd の domain を指すのに要る。
    pub uid: u32,
    /// 子に渡す環境変数。OS が起こす監督者は shell を通らないので、
    /// 状態・設定の置き場を知らないまま上がってしまう。
    pub environment: Vec<(String, String)>,
}

/// 何を書いて、何を叩くか。
#[derive(Debug, Clone)]
pub struct Plan {
    pub kind: Kind,
    pub label: String,
    pub unit_path: PathBuf,
    pub unit_text: String,
    /// 焼き込んだ実行ファイル (出力に添える)。
    pub exe: PathBuf,
    /// 監督者が書く先 (OS がここへ流す)。
    pub log: PathBuf,
    /// unit ファイルを置き換える前に叩くもの (best-effort、載っていなければ断られる)。
    pub before_write: Vec<Step>,
    /// unit ファイルを書いた後に叩くもの。
    pub register: Vec<Step>,
    /// unit ファイルを消す前に叩くもの。
    pub unregister: Vec<Step>,
    /// unit ファイルを消した後に叩くもの (systemd の読み直し)。
    pub after_remove: Vec<Step>,
    pub start: Vec<Step>,
    pub stop: Vec<Step>,
    pub status: Step,
}

/// この OS の作法。
pub fn kind() -> Kind {
    if cfg!(target_os = "macos") {
        Kind::Launchd
    } else {
        Kind::Systemd
    }
}

pub fn plan(kind: Kind, env: &Env) -> Plan {
    match kind {
        Kind::Launchd => launchd(env),
        Kind::Systemd => systemd(env),
    }
}

fn launchd(env: &Env) -> Plan {
    let domain = format!("gui/{}", env.uid);
    let target = format!("{domain}/{LABEL}");
    let unit_path = env.unit_dir.join(format!("{LABEL}.plist"));
    Plan {
        kind: Kind::Launchd,
        label: LABEL.to_owned(),
        log: env.log.clone(),
        exe: env.exe.clone(),
        unit_text: plist(env),
        // 同じ label が載ったままだと bootstrap が断る。手書きの plist が
        // 載っている場合も含めて、置き換えるときは先に降ろす。
        before_write: vec![Step::new("launchctl", &["bootout", &target])],
        register: vec![Step::new(
            "launchctl",
            &["bootstrap", &domain, &unit_path.display().to_string()],
        )],
        unregister: vec![Step::new("launchctl", &["bootout", &target])],
        after_remove: Vec::new(),
        start: vec![Step::new("launchctl", &["kickstart", &target])],
        // 監督者は SIGTERM で子を順に畳んでから終わる。KeepAlive があるので
        // launchd が上げ直す — 止め切るのは `bootout` の仕事ではなく
        // `service unregister` の仕事。
        stop: vec![Step::new("launchctl", &["kill", "SIGTERM", &target])],
        status: Step::new("launchctl", &["print", &target]),
        unit_path,
    }
}

fn plist(env: &Env) -> String {
    let mut text = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n",
    );
    text.push_str(&format!(
        "  <key>Label</key>\n  <string>{}</string>\n",
        escape(LABEL)
    ));
    text.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for arg in [
        env.exe.display().to_string(),
        "daemon".to_owned(),
        "supervise".to_owned(),
    ] {
        text.push_str(&format!("    <string>{}</string>\n", escape(&arg)));
    }
    text.push_str("  </array>\n");
    if !env.environment.is_empty() {
        text.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (key, value) in &env.environment {
            text.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                escape(key),
                escape(value)
            ));
        }
        text.push_str("  </dict>\n");
    }
    text.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    text.push_str("  <key>KeepAlive</key>\n  <true/>\n");
    let log = escape(&env.log.display().to_string());
    for key in ["StandardOutPath", "StandardErrorPath"] {
        text.push_str(&format!("  <key>{key}</key>\n  <string>{log}</string>\n"));
    }
    text.push_str("</dict>\n</plist>\n");
    text
}

/// plist は XML なので、パスに `&` や `<` が居ると壊れる。
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn systemd(env: &Env) -> Plan {
    let unit_path = env.unit_dir.join(UNIT_NAME);
    let user = ["--user"];
    Plan {
        kind: Kind::Systemd,
        label: UNIT_NAME.to_owned(),
        log: env.log.clone(),
        exe: env.exe.clone(),
        unit_text: service_unit(env),
        // systemd は書き換えたファイルを読み直せば済む。降ろす必要はない。
        before_write: Vec::new(),
        register: vec![
            Step::new("systemctl", &[user[0], "daemon-reload"]),
            Step::new("systemctl", &[user[0], "enable", UNIT_NAME]),
            Step::new("systemctl", &[user[0], "restart", UNIT_NAME]),
        ],
        unregister: vec![Step::new("systemctl", &[user[0], "disable", UNIT_NAME])],
        after_remove: vec![Step::new("systemctl", &[user[0], "daemon-reload"])],
        start: vec![Step::new("systemctl", &[user[0], "start", UNIT_NAME])],
        stop: vec![Step::new("systemctl", &[user[0], "stop", UNIT_NAME])],
        status: Step::new(
            "systemctl",
            &[
                user[0],
                "show",
                UNIT_NAME,
                "--property=LoadState,ActiveState,MainPID,ExecMainStatus",
            ],
        ),
        unit_path,
    }
}

fn service_unit(env: &Env) -> String {
    let mut text = String::from(
        "[Unit]\n\
Description=llm-gateway supervisor\n\n\
[Service]\n",
    );
    text.push_str(&format!(
        "ExecStart={} daemon supervise\n",
        env.exe.display()
    ));
    for (key, value) in &env.environment {
        text.push_str(&format!("Environment={key}={value}\n"));
    }
    text.push_str("Restart=always\n");
    // journald が受けるが、launchd 側と同じ場所も読めるようにしておく。
    text.push_str(&format!(
        "StandardOutput=append:{}\nStandardError=append:{}\n",
        env.log.display(),
        env.log.display()
    ));
    text.push_str("\n[Install]\nWantedBy=default.target\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            exe: PathBuf::from("/opt/homebrew/bin/llm-gateway"),
            unit_dir: PathBuf::from("/home/u/LaunchAgents"),
            log: PathBuf::from("/home/u/state/llm-gateway/logs/supervise.log"),
            uid: 501,
            environment: vec![("XDG_STATE_HOME".to_owned(), "/home/u/state".to_owned())],
        }
    }

    /// plist が抱えるのは監督者 1 つ。台の名前は 1 つも出てこない
    /// (どの台を抱えるかは登録簿の話、DR-0028 決定 7)。
    #[test]
    fn the_plist_starts_the_supervisor_and_nothing_else() {
        let plan = plan(Kind::Launchd, &env());
        assert!(plan.unit_text.contains(
            "<array>\n    <string>/opt/homebrew/bin/llm-gateway</string>\n    \
<string>daemon</string>\n    <string>supervise</string>\n  </array>"
        ));
        assert!(plan.unit_text.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plan.unit_text.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(!plan.unit_text.contains("stable"), "{}", plan.unit_text);
    }

    /// OS が起こす監督者は shell を通らない。置き場を知らないまま上がると、
    /// 登録簿も認証情報も別の場所を見る。
    #[test]
    fn the_unit_carries_the_directories_the_supervisor_needs() {
        assert!(
            plan(Kind::Launchd, &env())
                .unit_text
                .contains("<key>XDG_STATE_HOME</key>\n    <string>/home/u/state</string>")
        );
        assert!(
            plan(Kind::Systemd, &env())
                .unit_text
                .contains("Environment=XDG_STATE_HOME=/home/u/state")
        );
    }

    /// 監督者が黙って死んだときに何も残らないと、原因を探す先が無い。
    #[test]
    fn both_streams_go_to_one_log() {
        let text = plan(Kind::Launchd, &env()).unit_text;
        for key in ["StandardOutPath", "StandardErrorPath"] {
            assert!(
                text.contains(&format!("\n  <key>{key}</key>\n  <string>")),
                "{text}"
            );
        }
        assert_eq!(
            text.matches("/home/u/state/llm-gateway/logs/supervise.log")
                .count(),
            2,
            "{text}"
        );
    }

    /// 既存の 2 台 (`com.kawaz.llm-gateway-*`) と別の名で載る。移行の途中で
    /// 両方が載っても、片方だけを外せる。
    #[test]
    fn the_label_does_not_collide_with_the_two_that_are_already_loaded() {
        let plan = plan(Kind::Launchd, &env());
        assert_eq!(plan.label, "jp.kawaz.llm-gateway.supervise");
        assert!(!plan.label.starts_with("com.kawaz.llm-gateway-"));
        assert!(
            plan.unit_path
                .ends_with("jp.kawaz.llm-gateway.supervise.plist")
        );
    }

    /// 叩く命令は user domain のものだけ。`sudo` も system domain も要らない。
    #[test]
    fn every_launchctl_call_stays_in_the_user_domain() {
        let plan = plan(Kind::Launchd, &env());
        let calls: Vec<Vec<String>> = [
            plan.before_write.clone(),
            plan.register.clone(),
            plan.unregister.clone(),
            plan.start.clone(),
            plan.stop.clone(),
            vec![plan.status.clone()],
        ]
        .concat()
        .iter()
        .map(|s| {
            let mut line = vec![s.program.clone()];
            line.extend(s.args.clone());
            line
        })
        .collect();

        for call in &calls {
            assert_eq!(call[0], "launchctl", "{call:?}");
            assert!(
                call.iter().any(|a| a.contains("gui/501")),
                "not in the user domain: {call:?}"
            );
        }
        assert_eq!(
            calls[0],
            vec![
                "launchctl",
                "bootout",
                "gui/501/jp.kawaz.llm-gateway.supervise"
            ]
        );
        assert_eq!(
            calls[1],
            vec![
                "launchctl",
                "bootstrap",
                "gui/501",
                "/home/u/LaunchAgents/jp.kawaz.llm-gateway.supervise.plist"
            ]
        );
        assert_eq!(
            plan.stop[0].args,
            vec!["kill", "SIGTERM", "gui/501/jp.kawaz.llm-gateway.supervise"]
        );
    }

    /// systemd は unit ファイルを書いただけでは気づかない。
    #[test]
    fn systemd_is_told_to_read_the_unit_again() {
        let plan = plan(Kind::Systemd, &env());
        assert_eq!(plan.register[0].args, vec!["--user", "daemon-reload"]);
        assert_eq!(
            plan.register[1].args,
            vec!["--user", "enable", "llm-gateway-supervise.service"]
        );
        // 書き換えた unit で走り直させる (古い定義のまま生き続けない)。
        assert_eq!(
            plan.register[2].args,
            vec!["--user", "restart", "llm-gateway-supervise.service"]
        );
        // 消した後にも読み直させる (消えたことに気づかせる)。
        assert_eq!(plan.after_remove[0].args, vec!["--user", "daemon-reload"]);
        assert!(
            plan.unit_text
                .contains("ExecStart=/opt/homebrew/bin/llm-gateway daemon supervise")
        );
    }

    /// パスに XML の記号が入っていても plist が壊れない。
    #[test]
    fn a_path_with_xml_characters_is_escaped() {
        let mut env = env();
        env.exe = PathBuf::from("/home/a&b/llm-gateway");
        assert!(
            plan(Kind::Launchd, &env)
                .unit_text
                .contains("<string>/home/a&amp;b/llm-gateway</string>")
        );
    }

    #[test]
    fn a_step_prints_as_the_line_you_would_type() {
        assert_eq!(
            Step::new("launchctl", &["print", "gui/501/x"]).to_json(),
            json!(["launchctl", "print", "gui/501/x"])
        );
    }
}
