//! 置き場へ落とすときの共通の作法。
//!
//! 日次集計 ([`crate::stats`]) と利用状況のスナップショット ([`crate::quota`])
//! は、同じ置き場に**書き手ごとのファイル**を持つ。書き手の名前の付け方と
//! 「途中の状態を読ませない書き方」は両方で同じでなければならないので、
//! ここに 1 つだけ置く。

use std::path::Path;

use serde::Serialize;

/// 書き手の名前をファイル名に使える形にする。
///
/// `127.0.0.1:8402` のような待ち受け先がそのまま来る。`.` はファイル名の
/// 区切りに使っているので、混ざると名前の切り出しが狂う。
pub(crate) fn sanitize_writer(writer: &str) -> String {
    let cleaned: String = writer
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// 一時ファイルに振る通し番号。
///
/// pid だけでは、同じプロセスの 2 者が同じ名前を掴みうる。呼ぶ側は書き手を
/// 1 人に絞っているが、名前も重ならないようにして二重に塞ぐ (排他が緩んだ
/// ときに壊れ方が静かになるのを避ける)。
static NEXT_TEMPORARY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 一時ファイルに書いてから rename する。
///
/// 読む側 (CLI / 別プロセス) が途中の状態を読まないようにするため。名前に pid と
/// 通し番号を混ぜるのは、同じ置き場を使う別プロセス・同じプロセスの別の書き手と
/// 一時ファイルを取り違えないため (DR-0010 と同じ理由)。
///
/// 一時ファイル名は `<path>.tmp.<pid>.<連番>`。片付ける側 ([`sweep_temporaries`])
/// はこの形を当てにする。
pub(crate) fn write_atomically<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::Ordering;

    let seq = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}.{seq}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let json = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        // rename する前にディスクへ落とす。省くと、クラッシュ時に「rename は
        // 済んだが中身が空」のファイルが残りうる。
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// 前回の保存が途中で落ちて残った、自分の一時ファイルを消す。
///
/// rename まで進めなかった残骸は誰も読まないが、置き場に溜まり続ける。消すのは
/// **自分の名前が付いたものだけ** — 他の writer の一時ファイルは今まさに書いて
/// いる途中かもしれない。起動時に呼ぶ前提 (書いている最中に呼ぶと自分の書き
/// かけを消す)。
pub(crate) fn sweep_temporaries(dir: &Path, writer: &str) {
    // `2026-07-30.8402.json.tmp.<pid>.<連番>` の後半で見分ける。
    let mark = format!("{writer}.json.tmp.");
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.contains(&mark) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "書き損じを片付けました"),
            Err(e) => tracing::warn!(path = %path.display(), %e, "書き損じを消せません"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 待ち受け先がそのまま来ても、ファイル名の区切りを壊さない。
    #[test]
    fn a_listen_address_becomes_a_usable_name() {
        assert_eq!(sanitize_writer("127.0.0.1:8402"), "127-0-0-1-8402");
        assert_eq!(sanitize_writer("8402"), "8402");
    }

    /// 名前として何も残らない書き手でも、ファイル名は作れる。
    #[test]
    fn a_nameless_writer_still_gets_a_name() {
        assert_eq!(sanitize_writer(":::"), "unknown");
        assert_eq!(sanitize_writer(""), "unknown");
    }

    /// 書き上がりだけが見える。一時ファイルは残らない。
    #[test]
    fn a_write_leaves_only_the_finished_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-latest.8402.json");
        write_atomically(&path, &serde_json::json!({"a": 1})).unwrap();

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["usage-latest.8402.json".to_owned()]);
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"a\""));
    }

    /// 書き損じは自分の分だけ消す。
    ///
    /// 他の writer の一時ファイルは、今まさに書いている途中かもしれない。
    #[test]
    fn only_our_own_leftovers_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("usage-latest.8402.json.tmp.1234.0");
        let also_mine = dir.path().join("2026-07-30.8402.json.tmp.1234.1");
        let theirs = dir.path().join("2026-07-30.8401.json.tmp.9999.0");
        let finished = dir.path().join("2026-07-30.8402.json");
        for path in [&mine, &also_mine, &theirs, &finished] {
            std::fs::write(path, "{}").unwrap();
        }

        sweep_temporaries(dir.path(), "8402");

        assert!(!mine.exists());
        assert!(!also_mine.exists());
        assert!(theirs.exists(), "他の writer の書きかけは触らない");
        assert!(finished.exists(), "書き上がりは消さない");
    }

    /// 置き場が無くても落ちない (まだ 1 度も書いていない状態)。
    #[test]
    fn sweeping_a_missing_directory_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        sweep_temporaries(&dir.path().join("not-yet"), "8402");
    }
}
