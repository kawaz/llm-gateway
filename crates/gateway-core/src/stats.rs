//! 日次の集計を、書き手ごとのファイルに置いて読むときに合わせる器 (DR-0011)。
//!
//! 1 件ごとにディスクへ書くのは無駄なので、メモリに積んで定期的に落とす。
//! 書き込み先は**この書き手専用のファイル** (`<日付>.<書き手>.json`) にして
//! あり、複数のプロセスが並走しても互いのファイルを触らない。排他は要らない —
//! 読む側が全ファイルを足し合わせる。
//!
//! 落とすのは**変わった日だけ**で、書き手はプロセス内で 1 人に絞る。読み戻すのは
//! 当日と前日だけ (それ以前は閲覧時にファイルから読む)。
//!
//! 1 日分の値 `C` の形と、何をどう数えるかは利用側が決める。ここが持つのは
//! 日の振り分け、ファイルの置き方、合わせ方だけ。合わせ方の規則は
//! [`Mergeable`] (DR-0031 §2 (3) の「可換・結合的で、単位元を持つ」)。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::credential::time::{local_date, parse_date};
use crate::persist::{sanitize_writer, sweep_temporaries, write_atomically};

/// 足し合わせられる集計。
///
/// `merge` は可換・結合的で、[`Default`] が単位元であること。これで書き手の
/// 数や読む順序に依らず同じ合計になる。
pub trait Mergeable: Default + Clone {
    fn merge(&mut self, other: &Self);

    /// 何も数えていないか。空の日は読み戻さない。
    fn is_empty(&self) -> bool;
}

/// 鍵ごとの集計の集まりは、鍵ごとに足し合わせる。
impl<K: Ord + Clone, V: Mergeable> Mergeable for BTreeMap<K, V> {
    fn merge(&mut self, other: &Self) {
        for (key, value) in other {
            self.entry(key.clone()).or_default().merge(value);
        }
    }

    fn is_empty(&self) -> bool {
        BTreeMap::is_empty(self)
    }
}

/// 合わせた結果と、読めずに欠けた書き手 (DR-0031 §2 (3))。
///
/// 閲覧のように欠けを許す読み手は `missing` を見なくてよい。欠けがあると
/// 判定できない読み手 (枠の判定など) は、空でなければ判定を諦める。
#[derive(Debug, Clone, PartialEq)]
pub struct Merged<T> {
    pub value: T,
    /// 読めなかったファイルの書き手の名前 (重複なし、名前順)。
    pub missing: Vec<String>,
}

/// 起動時にメモリへ載せる日数 (当日から数えて)。
///
/// 常駐したまま日を跨ぐと当日分と前日分の両方に積むことがある (記録の日付は
/// 始まった時刻で決まるので、深夜に始まった処理が明けてから終わる)。載せるのは
/// **書き足す予定のある日**だけでよく、それ以前は閲覧時にファイルから読める。
/// 全部載せると、運用が続くほど起動時の読み込みと保存の対象が増える。
const RESTORED_DAYS: usize = 2;

/// 閲覧で遡れる日数の上限 (約 100 年)。
///
/// 上限を置くのは、`days` を秒に直す掛け算が桁あふれするため。`usize` の上限を
/// そのまま渡されると `i64` へ落とす時点で負に回り、絞り込みの起点が未来に
/// なって**全部消える**。これより長い期間を指したいなら `days = 0` (全期間)。
pub const MAX_DAYS: usize = 36_500;

/// 日ごとに積む器。
///
/// 積む側は await できない場所から呼ばれることがあるので同期の [`Mutex`] を
/// 使う。押さえている間にやるのは足し算だけ。
pub struct Stats<C> {
    counts: Mutex<BTreeMap<String, C>>,
    /// 前回落としてから変わった日。**日ごと**に持つ。
    ///
    /// 全体で 1 つの目印にすると、1 件積むだけで「メモリに載っている全部の日」を
    /// 書き直すことになる。過去日のファイルは読むだけにしたいので、変わった日を
    /// 名指しで覚える。
    dirty: Mutex<BTreeSet<String>>,
    /// 書き込み中であることの札。
    ///
    /// 定期の保存と終了時の保存が重なると、同じ一時ファイルを 2 者が切り詰め
    /// 合って「混ざった中身が rename される」「片方が消したファイルをもう
    /// 片方が rename しようとして失敗する」経路が開く。書く側を 1 人に絞る。
    writing: Mutex<()>,
    dir: PathBuf,
    /// このプロセスの書き込み先を他と分ける名前。
    writer: String,
}

impl<C> Stats<C>
where
    C: Mergeable + Serialize + DeserializeOwned,
{
    /// 置き場と書き手の名前を決めて作る。
    ///
    /// 起動時に自分のファイルを読み戻すのは呼び出し側 ([`Self::restore`])。
    pub fn new(dir: impl Into<PathBuf>, writer: &str) -> Self {
        Self {
            counts: Mutex::new(BTreeMap::new()),
            dirty: Mutex::new(BTreeSet::new()),
            writing: Mutex::new(()),
            dir: dir.into(),
            writer: sanitize_writer(writer),
        }
    }

    /// `at_secs` (**unix 秒**) の日の集計を `add` で書き換える。
    ///
    /// 日付はこの時刻の地方時で決める。日を跨いだら新しい日付の欄に積むだけで、
    /// 落とす側が日ごとのファイルへ振り分ける。ミリ秒を渡すと日付が 5 桁の年へ
    /// 飛び、その 1 本が集計から迷子になる。
    pub fn add(&self, at_secs: i64, add: impl FnOnce(&mut C)) {
        let date = local_date(at_secs);

        // メモリに無い日なら、その日の自分のファイルを先に読む。読まずに積むと
        // 次の保存が**その日のファイルを上書きして消す**。読み戻しの範囲
        // ([`RESTORED_DAYS`]) の外に落ちた日へ積む場合 (時計が巻き戻った等) も、
        // ここで拾えば失われない。ファイルを読むのは鍵を持つ前 (I/O を
        // 押さえた中でやらない)。
        let seed = if self
            .counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&date)
        {
            None
        } else {
            self.read_own_day(&date)
        };

        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        add(counts
            .entry(date.clone())
            .or_insert_with(|| seed.unwrap_or_default()));
        drop(counts);

        self.dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(date);
    }

    /// メモリに積んである分。
    pub fn in_memory(&self) -> BTreeMap<String, C> {
        self.counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 起動時に、自分が前回書いたファイルを読み戻す。
    ///
    /// これをやらないと、再起動のたびに当日分が 0 から数え直しになり、次の
    /// flush で**前回までの分を上書きして消す**。読むのは自分のファイルだけ
    /// (他の書き手の分は向こうが持っている)。
    ///
    /// 載せるのは直近 [`RESTORED_DAYS`] 日分。それ以前の自分のファイルは
    /// 触らないまま、閲覧では読み込まれる ([`Self::merged`])。
    ///
    /// 積み始める前に 1 回だけ呼ぶ前提。積み始めた後に呼ぶと、読み戻した
    /// 日についてはメモリの積み分が読み戻しで置き換わる。
    /// `now` を引数で受けるのは、読み戻す「当日」を試験から固定するため
    /// (実時計に縛ると、固定時刻で積んだデータが日付の進みで範囲外になる)。
    pub fn restore(&self, now: i64) {
        sweep_temporaries(&self.dir, &self.writer);
        self.absorb_millisecond_dates();

        let recent: Vec<String> = (0..RESTORED_DAYS as i64)
            .map(|back| local_date(now - back * 86_400))
            .collect();

        let mut restored = BTreeMap::new();
        for date in recent {
            if let Some(day) = self.read_own_day(&date) {
                restored.insert(date, day);
            }
        }
        if restored.is_empty() {
            return;
        }
        let days = restored.len();
        *self.counts.lock().unwrap_or_else(|e| e.into_inner()) = restored;
        tracing::info!(days, "loaded daily totals from disk");
    }

    /// ミリ秒を秒として数えた日付のファイルを、本来の日へ寄せる。
    ///
    /// 時刻をミリ秒のまま積んでいた頃の置き土産で、`58667-10-30.…json` の
    /// ような 5 桁の年のファイルが残る。日付として読めない名前なので閲覧は
    /// 素通りするが、置き場に溜まり続けるうえ、その 1 本分の記録が集計から
    /// 落ちたままになる。
    ///
    /// Design rationale: 直す口を別に生やさず、読み戻しの一部として黙って
    /// 済ませる。ここは既に「古い形のファイルを読んで新しい形で書き戻す」
    /// 移行を通してきた場所で、運用者が手順を覚える必要のない側に揃える。
    /// 直す対象が無ければ何もしない。
    ///
    /// 日付は `日数 × 86400` がミリ秒だったので、1000 で割れば本来の時刻に
    /// 戻る (地方時の時差の分だけずれるが、1 日の中に収まる)。寄せ先は
    /// **自分のファイル**。書き手の名前はファイルを分けるためだけの目印で
    /// 集計には出ないので、他の書き手のファイルへ書きに行って、向こうの
    /// 保存と潰し合う方が高くつく。取り込む前に名前を変えて自分のものに
    /// するのは、2 つのプロセスが同時に立ち上がったときに同じ 1 本を
    /// 両方が数えないようにするため。
    fn absorb_millisecond_dates(&self) {
        let mut absorbed = 0usize;
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(millis) = millisecond_date_of_file(name) else {
                continue;
            };
            // 寄せ先を先に読む。読めない寄せ先へ書くと、そこにある 1 日分を
            // 取り込んだつもりで踏み潰す。読めないうちは寄せずに残しておく。
            let target = self.path_of(&local_date(millis.div_euclid(1000)));
            let mut merged: C = match read_day(&target) {
                Ok(day) => day,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => C::default(),
                Err(e) => {
                    tracing::warn!(path = %target.display(), %e, "cannot read daily totals");
                    continue;
                }
            };

            // 名前を変えて取り込む者を 1 人に決める。負けた側は消えた名前を
            // 読もうとして諦めるだけ。
            let claimed = path.with_file_name(format!("{name}.absorbing.{}", std::process::id()));
            if std::fs::rename(&path, &claimed).is_err() {
                continue;
            }
            let day: C = match read_day(&claimed) {
                Ok(day) => day,
                Err(e) => {
                    tracing::warn!(path = %claimed.display(), %e, "cannot read daily totals");
                    continue;
                }
            };
            merged.merge(&day);
            if let Err(e) = write_atomically(&target, &merged) {
                tracing::warn!(path = %target.display(), %e, "cannot write daily totals");
                continue;
            }
            let _ = std::fs::remove_file(&claimed);
            absorbed += 1;
        }
        if absorbed > 0 {
            tracing::info!(
                files = absorbed,
                "moved daily totals that were filed under a millisecond date"
            );
        }
    }

    /// 変わった日だけをディスクへ落とす。変わっていなければ何もしない。
    ///
    /// 書くのは自分のファイルだけ。日付ごとに分けて書くので、日を跨いだ直後に
    /// 残っている前日分もそのまま正しい先へ行く。
    ///
    /// 失敗しても積んだ分は失わない (次の周回で書き直す)。呼び出し側は警告を
    /// 残して進めばよい (best-effort、DR-0031 §2 (3))。
    pub fn flush(&self) -> std::io::Result<()> {
        // 先に目印を外す。書いている間に積まれた分は、次の周回で拾い直せる
        // よう積み直される (取りこぼしより書き直しの方が安い)。
        let pending: Vec<String> =
            std::mem::take(&mut *self.dirty.lock().unwrap_or_else(|e| e.into_inner()))
                .into_iter()
                .collect();
        if pending.is_empty() {
            return Ok(());
        }

        // 書く者を 1 人に絞る。ここから下は直列。
        let _writing = self.writing.lock().unwrap_or_else(|e| e.into_inner());

        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            self.mark_dirty(pending);
            return Err(e);
        }
        for (i, date) in pending.iter().enumerate() {
            let Some(day) = self
                .counts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(date)
                .cloned()
            else {
                continue;
            };
            if let Err(e) = write_atomically(&self.path_of(date), &day) {
                // 書けなかった日と、まだ書いていない日を積み直す。
                self.mark_dirty(pending[i..].to_vec());
                return Err(e);
            }
        }
        Ok(())
    }

    /// この日を「変わった」に戻す。保存し損なった分を次の周回へ回す。
    fn mark_dirty(&self, dates: Vec<String>) {
        let mut dirty = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        dirty.extend(dates);
    }

    /// 全書き手のファイルとメモリの分を日ごとに合わせた全体像。
    ///
    /// 落とす前の分もここに出る。閲覧が「さっき数えた分が出ない」にならない
    /// ようにするため。ただし**他の書き手がまだ落としていない分は見えない**
    /// (向こうの保存間隔だけ遅れて現れる)。
    ///
    /// `days` が 0 でなければ、`now_secs` の日から遡って `days` 日分に絞る。
    pub fn merged(&self, days: usize, now_secs: i64) -> Merged<BTreeMap<String, C>> {
        let mine = self.in_memory();
        // メモリに載っている日は、自分のファイルより新しい。その日だけ
        // 自分のファイルを読み飛ばす (両方足すと二重に数える)。読み戻しの
        // 範囲外の過去日は、メモリに無いのでファイルから読む。
        let superseded: BTreeSet<&str> = mine.keys().map(String::as_str).collect();

        let Merged {
            value: mut merged,
            missing,
        } = self.on_disk(&superseded);
        for (date, day) in &mine {
            merged.entry(date.clone()).or_default().merge(day);
        }

        // 直近 N 日に絞る。日付は文字列だが `YYYY-MM-DD` は辞書順が日付順。
        // 上限で抑えてから秒に直す (抑えないと桁あふれで起点が未来に回る)。
        if days > 0 {
            let back = (days.min(MAX_DAYS) as i64 - 1).saturating_mul(86_400);
            let from = local_date(now_secs.saturating_sub(back));
            merged.retain(|date, _| date.as_str() >= from.as_str());
        }
        Merged {
            value: merged,
            missing,
        }
    }

    /// ディスクにある分を日付ごとに合わせる。
    ///
    /// `superseded` に挙げた日については、自分のファイルを読み飛ばす
    /// (メモリの方が新しい)。他の書き手のファイルは常に読む。
    fn on_disk(&self, superseded: &BTreeSet<&str>) -> Merged<BTreeMap<String, C>> {
        let mut merged: BTreeMap<String, C> = BTreeMap::new();
        let mut missing = BTreeSet::new();
        let own = self.own_suffix();
        for (date, path) in self.day_files() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(&own) && superseded.contains(date.as_str()) {
                continue;
            }
            let Ok(day) = read_day::<C>(&path) else {
                tracing::warn!(path = %path.display(), "cannot read daily totals");
                missing.insert(writer_of_file(name).to_owned());
                continue;
            };
            merged.entry(date).or_default().merge(&day);
        }
        Merged {
            value: merged,
            missing: missing.into_iter().collect(),
        }
    }

    /// 自分が書いたその日のファイル。無い / 読めない / 空なら `None`。
    fn read_own_day(&self, date: &str) -> Option<C> {
        let path = self.path_of(date);
        if !path.exists() {
            return None;
        }
        match read_day::<C>(&path) {
            Ok(day) if !day.is_empty() => Some(day),
            Ok(_) => None,
            Err(e) => {
                // 読めない 1 日分で起動や集計を止めない。
                tracing::warn!(path = %path.display(), %e, "cannot read daily totals");
                None
            }
        }
    }

    /// 置き場にある日次ファイルの `(日付, パス)`。日付として読めない名前は無視する。
    fn day_files(&self) -> Vec<(String, PathBuf)> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(date) = date_of_file(name) {
                found.push((date, path));
            }
        }
        found
    }

    fn own_suffix(&self) -> String {
        format!(".{}.json", self.writer)
    }

    /// 自分が書くその日のファイル。
    pub fn path_of(&self, date: &str) -> PathBuf {
        self.dir.join(format!("{date}.{}.json", self.writer))
    }
}

/// ファイル名から日付を取り出す。`2026-07-30.8402.json` → `2026-07-30`。
///
/// 形が合わないものは無視する。置き場に紛れ込んだ別のファイルを日付として
/// 読むと、ありえない日付が一覧に出る。
pub fn date_of_file(name: &str) -> Option<String> {
    if !name.ends_with(".json") {
        return None;
    }
    let date = name.split('.').next()?;
    let ok = date.len() == 10
        && date.as_bytes().iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    ok.then(|| date.to_owned())
}

/// 日次ファイルの名前から書き手の名前。`2026-07-30.8402.json` → `8402`。
fn writer_of_file(name: &str) -> &str {
    name.strip_suffix(".json")
        .and_then(|stem| stem.split_once('.'))
        .map_or(name, |(_, writer)| writer)
}

/// ミリ秒を秒として数えた日付のファイルなら、その元の時刻 (unix ミリ秒)。
///
/// `58667-10-30.8402.json` → `58667-10-30` の 00:00 を数にしたもの。見分ける
/// のは**年が 4 桁に収まらないこと**。1 万年先の日付を本気で書いたファイルは
/// 無いので、これだけで足りる。[`date_of_file`] が拾う形 (4 桁の年) はここでは
/// 拾わない — 素性の正しい日次ファイルを動かしてはいけない。
pub fn millisecond_date_of_file(name: &str) -> Option<i64> {
    if !name.ends_with(".json") {
        return None;
    }
    let date = name.split('.').next()?;
    if date.split('-').next()?.len() <= 4 {
        return None;
    }
    parse_date(date)
}

/// 日次ファイル 1 本を読む。
pub fn read_day<C: DeserializeOwned>(path: &Path) -> std::io::Result<C> {
    let raw = std::fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-29T12:00:00Z
    const NOW: i64 = 1_785_326_400;

    #[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize)]
    struct Count(u64);

    impl Mergeable for Count {
        fn merge(&mut self, other: &Self) {
            self.0 += other.0;
        }
        fn is_empty(&self) -> bool {
            self.0 == 0
        }
    }

    type Day = BTreeMap<String, Count>;

    #[test]
    fn maps_merge_key_by_key() {
        let mut a: Day = [("x".to_owned(), Count(1))].into();
        let b: Day = [("x".to_owned(), Count(2)), ("y".to_owned(), Count(3))].into();
        Mergeable::merge(&mut a, &b);
        assert_eq!(a["x"], Count(3));
        assert_eq!(a["y"], Count(3));
        assert!(!Mergeable::is_empty(&a));
    }

    /// 読めない書き手のファイルは飛ばし、その書き手を `missing` に挙げる。
    #[test]
    fn an_unreadable_writer_is_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let other = Stats::<Day>::new(dir.path(), "a");
        other.add(NOW, |d| d.entry("x".to_owned()).or_default().0 += 2);
        other.flush().unwrap();
        std::fs::write(
            dir.path().join(format!("{}.b.json", local_date(NOW))),
            "{ broken",
        )
        .unwrap();

        let mine = Stats::<Day>::new(dir.path(), "c");
        mine.add(NOW, |d| d.entry("x".to_owned()).or_default().0 += 1);
        let merged = mine.merged(1, NOW);
        assert_eq!(merged.value[&local_date(NOW)]["x"], Count(3));
        assert_eq!(merged.missing, vec!["b".to_owned()]);
    }

    #[test]
    fn the_writer_is_read_from_the_file_name() {
        assert_eq!(writer_of_file("2026-07-30.8402.json"), "8402");
    }
}
