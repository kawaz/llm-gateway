//! 日ごと・書き手ごとのファイルに、1 行 1 件で追記する器。
//!
//! 一度書いた行は書き換えない。書き込み先は**この書き手専用のファイル**
//! (`<日付>.<書き手>.jsonl`) で、複数のプロセスが並走しても互いのファイルを
//! 触らない。排他は要らない — 読む側が全書き手の行を合わせる (和集合で、
//! 順序も書き手の数も結果を変えない。DR-0031 §2 (3) の合わせ方の規則)。
//!
//! 1 件ごとにディスクへ書くのは無駄なので、メモリに溜めて定期的に落とす
//! ([`DailyLog::flush`])。落とし損ねた行は次の周回で書き直す (best-effort)。
//!
//! 1 行の形と、いつ行を足すかは利用側が決める。ここが持つのは日の振り分け、
//! ファイルの置き方、追記の作法だけ。
//!
//! 古い日のファイルは消さない。読み手は欠けた日を「行が無い」として読めば
//! よいので、手で消しても壊れない。

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use crate::credential::time::local_date;
use crate::persist::sanitize_writer;

/// 日ごとに追記する器。
///
/// 行を足す側は await できない場所から呼ばれることがあるので同期の [`Mutex`]
/// を使う。押さえている間にやるのは行の付け足しだけ。
pub struct DailyLog {
    /// まだ落としていない行。日付 → その日の行 (足した順)。
    pending: Mutex<BTreeMap<String, Vec<String>>>,
    /// 書き込み中であることの札。
    ///
    /// 定期の保存と終了時の保存が重なると、同じ行を 2 者が書いて行が倍になる。
    /// 書く側を 1 人に絞る ([`crate::stats::Stats`] と同じ)。
    writing: Mutex<()>,
    dir: PathBuf,
    /// このプロセスの書き込み先を他と分ける名前。
    writer: String,
}

impl DailyLog {
    /// 置き場と書き手の名前を決めて作る。置き場は最初に落とすときに作る。
    pub fn new(dir: impl Into<PathBuf>, writer: &str) -> Self {
        Self {
            pending: Mutex::new(BTreeMap::new()),
            writing: Mutex::new(()),
            dir: dir.into(),
            writer: sanitize_writer(writer),
        }
    }

    /// `at_secs` (**unix 秒**) の日の行として 1 件足す。
    ///
    /// 日付はこの時刻の地方時で決める ([`crate::stats::Stats::add`] と同じ)。
    /// 1 件を 1 行の JSON にできなければ、その 1 件だけ捨てて警告を残す
    /// (書けない行のために他の行を止めない)。
    pub fn push<T: Serialize>(&self, at_secs: i64, entry: &T) {
        let line = match serde_json::to_string(entry) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!(%e, "cannot encode a log line");
                return;
            }
        };
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(local_date(at_secs))
            .or_default()
            .push(line);
    }

    /// 溜めた行を日ごとのファイルへ追記する。溜めていなければ何もしない。
    ///
    /// 書けなかった日と、まだ書いていない日の行は溜め直して次の周回へ回す。
    /// 呼び出し側は警告を残して進めばよい (best-effort)。
    pub fn flush(&self) -> std::io::Result<()> {
        // 先に抜き取る。書いている間に足された行は次の周回で書く。
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        if pending.is_empty() {
            return Ok(());
        }

        // 書く者を 1 人に絞る。ここから下は直列。
        let _writing = self.writing.lock().unwrap_or_else(|e| e.into_inner());

        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            self.requeue(pending);
            return Err(e);
        }
        let mut days = pending.into_iter();
        while let Some((date, lines)) = days.next() {
            if let Err(e) = append_lines(&self.path_of(&date), &lines) {
                let mut rest: BTreeMap<String, Vec<String>> = days.collect();
                rest.insert(date, lines);
                self.requeue(rest);
                return Err(e);
            }
        }
        Ok(())
    }

    /// 書けなかった行を、後から足された行より前に戻す (足した順を保つ)。
    fn requeue(&self, failed: BTreeMap<String, Vec<String>>) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for (date, mut lines) in failed {
            let later = pending.remove(&date).unwrap_or_default();
            lines.extend(later);
            pending.insert(date, lines);
        }
    }

    /// 自分が書くその日のファイル。
    pub fn path_of(&self, date: &str) -> PathBuf {
        self.dir.join(format!("{date}.{}.jsonl", self.writer))
    }
}

/// ファイルの末尾に行を足す。
///
/// 前回の書き込みが行の途中で途切れていたら (書いている最中に落ちた)、先に
/// 改行で閉じる。閉じずに続けると、途切れた行と次の行が 1 行に繋がって両方
/// 読めなくなる。途切れた行そのものは読み手が読み飛ばす。
fn append_lines(path: &Path, lines: &[String]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?;

    let mut text = String::new();
    let len = file.metadata()?.len();
    if len > 0 {
        let mut last = [0u8; 1];
        file.read_exact_at(&mut last, len - 1)?;
        if last[0] != b'\n' {
            text.push('\n');
        }
    }
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    // まとめて 1 度で書く。行ごとに書くと、途中で落ちた時に途切れる箇所が増える。
    file.write_all(text.as_bytes())?;
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-29T12:00:00Z を unix 秒で。
    const NOW: i64 = 1_785_326_400;
    const DAY: i64 = 86_400;

    fn lines_of(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// 足した行は、その時刻の地方時の日のファイルへ、足した順に 1 行ずつ落ちる。
    ///
    /// 日を跨いだ行は翌日のファイルへ行く。読み手は日付のファイル名で期間を
    /// 絞るので、行の時刻と置かれる日がずれると絞り込みから漏れる。
    #[test]
    fn lines_land_in_the_file_of_their_day_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = DailyLog::new(dir.path(), "8402");

        log.push(NOW, &serde_json::json!({"n": 1}));
        log.push(NOW + 1, &serde_json::json!({"n": 2}));
        log.push(NOW + DAY, &serde_json::json!({"n": 3}));
        log.flush().unwrap();

        assert_eq!(
            lines_of(&log.path_of(&local_date(NOW))),
            vec![r#"{"n":1}"#, r#"{"n":2}"#]
        );
        assert_eq!(
            lines_of(&log.path_of(&local_date(NOW + DAY))),
            vec![r#"{"n":3}"#]
        );
    }

    /// 何も足していなければ書かない。空のファイルも置き場も作らない。
    #[test]
    fn nothing_is_written_without_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = DailyLog::new(dir.path().join("not-yet"), "8402");
        log.flush().unwrap();
        assert!(!dir.path().join("not-yet").exists());
    }

    /// 落とすたびに書き足す。前に書いた行を上書きしない。
    ///
    /// 再起動 (同じ書き手の新しい器) の後も、その日のファイルの続きに足す。
    /// 上書きすると、それまでの行が二度と戻らない (作り直せない記録)。
    #[test]
    fn each_flush_appends_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let log = DailyLog::new(dir.path(), "8402");
            log.push(NOW, &1);
            log.flush().unwrap();
            log.push(NOW, &2);
            log.flush().unwrap();
            // 2 度目の flush は、新しい行が無ければ何も足さない。
            log.flush().unwrap();
        }
        let log = DailyLog::new(dir.path(), "8402");
        log.push(NOW, &3);
        log.flush().unwrap();

        assert_eq!(
            lines_of(&log.path_of(&local_date(NOW))),
            vec!["1", "2", "3"]
        );
    }

    /// 書き手ごとに別のファイルへ書く。他の書き手のファイルは触らない。
    ///
    /// 同じ置き場を複数のプロセスが共有しても、互いの行を混ぜず排他も要らない。
    #[test]
    fn writers_keep_their_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = DailyLog::new(dir.path(), "127.0.0.1:8401");
        let b = DailyLog::new(dir.path(), "127.0.0.1:8402");
        a.push(NOW, &"a");
        b.push(NOW, &"b");
        a.flush().unwrap();
        b.flush().unwrap();

        let date = local_date(NOW);
        assert_eq!(
            a.path_of(&date).file_name().unwrap(),
            format!("{date}.127-0-0-1-8401.jsonl").as_str(),
            "the listen address becomes a usable file name"
        );
        assert_eq!(lines_of(&a.path_of(&date)), vec![r#""a""#]);
        assert_eq!(lines_of(&b.path_of(&date)), vec![r#""b""#]);
    }

    /// 書けなかった行は捨てずに、書ける状態になった次の周回で落ちる。
    ///
    /// 落とし損ねた間に足された行は、その後ろに並ぶ (足した順を保つ)。
    #[test]
    fn a_failed_flush_is_retried_in_order() {
        let dir = tempfile::tempdir().unwrap();
        // 置き場と同じ名前のファイルを置いて、ディレクトリを作れなくする。
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();

        let log = DailyLog::new(&blocked, "8402");
        log.push(NOW, &1);
        assert!(log.flush().is_err(), "fails because it cannot write");
        log.push(NOW, &2);

        std::fs::remove_file(&blocked).unwrap();
        log.flush().unwrap();
        assert_eq!(lines_of(&log.path_of(&local_date(NOW))), vec!["1", "2"]);
    }

    /// 前回の書き込みが行の途中で途切れていても、次の行は独立した 1 行になる。
    ///
    /// 途切れた行に続けて書くと、2 行が繋がって新しい行まで読めなくなる。
    #[test]
    fn a_torn_last_line_is_closed_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let log = DailyLog::new(dir.path(), "8402");
        let path = log.path_of(&local_date(NOW));
        std::fs::write(&path, "{\"n\":1}\n{\"n\":").unwrap();

        log.push(NOW, &serde_json::json!({"n": 2}));
        log.flush().unwrap();

        assert_eq!(
            lines_of(&path),
            vec![r#"{"n":1}"#, r#"{"n":"#, r#"{"n":2}"#]
        );
    }
}
