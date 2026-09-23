//! 汎用層に利用側の語彙が現れないことの判定 (DR-0030 §1、計画 gateway-core-split §3)。
//!
//! 試験もこの crate のコードなので、ファイル全文を読む。試験の値も中立な名前にする。

use std::path::{Path, PathBuf};

/// 現れてはいけない語 (小文字にしてから部分一致で照合)。
///
/// `token` は認証の語として汎用層が使うので入れない。
const WORDS: &[&str] = &[
    "model",
    "usage",
    "cache",
    "anthropic",
    "claude",
    "openai",
    "bedrock",
    "codex",
    "chatgpt",
];

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn scanned() -> Vec<PathBuf> {
    let mut files = Vec::new();
    rust_files(&src(), &mut files);
    files.sort();
    files
}

#[test]
fn core_never_uses_consumer_vocabulary() {
    let src = src();
    let mut leaks = Vec::new();

    for path in scanned() {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let name = path.strip_prefix(&src).unwrap_or(&path).display();

        for (number, line) in text.lines().enumerate() {
            let lowered = line.to_lowercase();
            for word in WORDS {
                if lowered.contains(word) {
                    leaks.push(format!("{name}:{}: {word} — {}", number + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        leaks.is_empty(),
        "gateway-core uses consumer vocabulary (DR-0030 §1):\n{}",
        leaks.join("\n")
    );
}

/// 走査が空振りしていないこと。
///
/// 置き場を取り違えたまま「漏れ 0 件」で通り続けると、判定が黙って効かなくなる。
#[test]
fn scan_reaches_the_sources() {
    let files = scanned();
    assert!(!files.is_empty(), "no .rs file under {}", src().display());
    assert!(
        files.iter().any(|p| p.ends_with("src/lib.rs")),
        "lib.rs not scanned"
    );
}
