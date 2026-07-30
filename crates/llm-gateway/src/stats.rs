//! 使用量の日次集計 (DR-0011)。
//!
//! upstream は消費したトークン数を**応答の本文に**載せて返す。ヘッダ由来の
//! 利用状況 ([`crate::usage`]) が「上限にどれだけ近いか」の今この瞬間の値なのに
//! 対して、こちらは「いつ・どの認証情報で・どのモデルに・どれだけ使ったか」を
//! 日ごとに積む。軸が直交しているので別の口にしてある。
//!
//! ## 本文を読むのに、中継は素通しのまま
//!
//! [`crate::relay`] は本文を解釈しない。SSE のバイト列がそのまま通り抜ける
//! ことが中継の柱なので、そこに usage の解釈を混ぜない。代わりに本文を
//! 覗くだけの変換層 ([`tap`]) を別に置き、受け取り口で中継の内側に挟む。
//! 挟んでも流れるバイト列は 1 バイトも変わらない。
//!
//! ## 数え方
//!
//! usage の値は**累積 (その応答の現時点での総量)** で届く。実測 (2026-07-30、
//! claude-haiku-4-5 / max_tokens 16 の streaming) では:
//!
//! - `message_start` の `/message/usage`: `input_tokens:18, output_tokens:1`
//! - `message_delta` の `/usage`: `input_tokens:18, output_tokens:16`
//!
//! 差分ではないので、**足さずに後から来た値で置き換える**。足すと
//! `1 + 16 = 17` になって実際より多く数える。`message_delta` は input と
//! cache も再掲するため、イベント名で拾う先を決めず「usage が載っていたら、
//! 載っているフィールドだけ上書き」で扱う。
//!
//! 読む単位は行ではなく**イベント**。同じイベントの連続する `data:` 行は改行で
//! 繋いだものが 1 つの中身なので (SSE の仕様)、空行まで溜めてから解く。
//!
//! ## 落ちても失わない
//!
//! 1 リクエストごとにディスクへ書くのは無駄なので、メモリに積んで定期的に
//! 落とす。書き込み先は**このプロセス専用のファイル** (`<日付>.<ポート>.json`)
//! にしてあり、複数の gateway が並走しても互いのファイルを触らない。排他は
//! 要らない — 読む側が全ファイルを足し合わせる。
//!
//! 落とすのは**変わった日だけ**で、書き手はプロセス内で 1 人に絞る。読み戻すのは
//! 当日と前日だけ (それ以前は閲覧時にファイルから読む)。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::credential::time::local_date;

/// 認証情報を持たない経路 (relay 型) の記録先。
///
/// 集計から落とさないのは、gateway 越しに使った分が行ごと消えると合計が
/// 合わなくなるため。認証情報の名前と衝突しない語を使う。
pub const NO_CREDENTIAL: &str = "-";

/// 1 応答から拾ったトークン数。
///
/// どれも欠けうる。`count_tokens` のように usage を返さない応答もあるので、
/// 「載っていなかった」と「0 だった」を [`Option`] で区別する。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_creation: Option<u64>,
    pub cache_read: Option<u64>,
}

impl Tokens {
    /// 何か 1 つでも拾えたか。1 つも無ければ記録しない。
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// usage オブジェクトに載っている分を取り込む。
    ///
    /// 値は累積で届くので**上書き**する (足さない)。載っていないフィールドは
    /// 前に拾った値を保つ — `message_delta` が一部しか載せない場合に備える。
    fn absorb(&mut self, usage: &serde_json::Value) {
        let take = |name: &str, slot: &mut Option<u64>| {
            if let Some(v) = usage.get(name).and_then(serde_json::Value::as_u64) {
                *slot = Some(v);
            }
        };
        take("input_tokens", &mut self.input);
        take("output_tokens", &mut self.output);
        take("cache_creation_input_tokens", &mut self.cache_creation);
        take("cache_read_input_tokens", &mut self.cache_read);
    }
}

/// 積み上がった数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counters {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl Counters {
    /// 1 応答分を足す。
    fn add(&mut self, t: &Tokens) {
        self.requests += 1;
        self.input_tokens += t.input.unwrap_or(0);
        self.output_tokens += t.output.unwrap_or(0);
        self.cache_creation_input_tokens += t.cache_creation.unwrap_or(0);
        self.cache_read_input_tokens += t.cache_read.unwrap_or(0);
    }

    /// 別の集計を足し込む。ファイルをまたいで合わせるときに使う。
    fn merge(&mut self, other: &Self) {
        self.requests += other.requests;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }
}

/// モデル名 → 集計。
pub type ByModel = BTreeMap<String, Counters>;
/// 認証情報 → モデル → 集計。1 日分のファイルの中身がこの形。
pub type ByCredential = BTreeMap<String, ByModel>;
/// 日付 → 認証情報 → モデル → 集計。閲覧に出す形。
pub type ByDate = BTreeMap<String, ByCredential>;

/// 起動時にメモリへ載せる日数 (当日から数えて)。
///
/// 常駐したまま日を跨ぐと当日分と前日分の両方に積むことがある (応答の日付は
/// 始まった時刻で決まるので、深夜に始まった応答が明けてから終わる)。載せるのは
/// **書き足す予定のある日**だけでよく、それ以前は閲覧時にファイルから読める。
/// 全部載せると、運用が続くほど起動時の読み込みと保存の対象が増える。
const RESTORED_DAYS: usize = 2;

/// 閲覧で遡れる日数の上限 (約 100 年)。
///
/// 上限を置くのは、`days` を秒に直す掛け算が桁あふれするため。`usize` の上限を
/// そのまま渡されると `i64` へ落とす時点で負に回り、絞り込みの起点が未来に
/// なって**全部消える**。これより長い期間を指したいなら `days = 0` (全期間)。
pub const MAX_DAYS: usize = 36_500;

/// 日ごと・認証情報ごと・モデルごとに積む器。
///
/// 書き込みは中継の終わり (tap の後始末) から呼ばれる。await できない場所なので
/// 同期の [`Mutex`] を使う。押さえている間にやるのは足し算だけ。
pub struct Stats {
    counts: Mutex<ByDate>,
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
    /// このプロセスの書き込み先を他と分ける名前 (待ち受けポート)。
    writer: String,
}

impl Stats {
    /// 置き場と書き手の名前を決めて作る。
    ///
    /// 起動時に自分のファイルを読み戻すのは呼び出し側 ([`Self::restore`])。
    pub fn new(dir: impl Into<PathBuf>, writer: &str) -> Self {
        Self {
            counts: Mutex::new(ByDate::new()),
            dirty: Mutex::new(BTreeSet::new()),
            writing: Mutex::new(()),
            dir: dir.into(),
            writer: sanitize_writer(writer),
        }
    }

    /// 1 応答分を積む。
    ///
    /// `at` はイベントを観測した時刻。日付はこの時刻の地方時で決める。日を
    /// 跨いだら新しい日付の欄に積むだけで、落とす側が日ごとのファイルへ
    /// 振り分ける。
    pub fn record(&self, at: i64, credential: Option<&str>, model: &str, tokens: &Tokens) {
        if tokens.is_empty() {
            return;
        }
        let date = local_date(at);
        let credential = credential.unwrap_or(NO_CREDENTIAL).to_owned();

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
        counts
            .entry(date.clone())
            .or_insert_with(|| seed.unwrap_or_default())
            .entry(credential)
            .or_default()
            .entry(model.to_owned())
            .or_default()
            .add(tokens);
        drop(counts);

        self.dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(date);
    }

    /// メモリに積んである分。
    pub fn in_memory(&self) -> ByDate {
        self.counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 起動時に、自分が前回書いたファイルを読み戻す。
    ///
    /// これをやらないと、再起動のたびに当日分が 0 から数え直しになり、次の
    /// flush で**前回までの分を上書きして消す**。読むのは自分のファイルだけ
    /// (他の writer の分は向こうが持っている)。
    ///
    /// 載せるのは直近 [`RESTORED_DAYS`] 日分。それ以前の自分のファイルは
    /// 触らないまま、閲覧では読み込まれる ([`Self::report`])。
    ///
    /// serve の開始前に 1 回だけ呼ぶ前提。積み始めた後に呼ぶと、読み戻した
    /// 日についてはメモリの積み分が読み戻しで置き換わる。
    pub fn restore(&self) {
        self.sweep_temporaries();

        let now = crate::credential::time::now_unix();
        let recent: Vec<String> = (0..RESTORED_DAYS as i64)
            .map(|back| local_date(now - back * 86_400))
            .collect();

        let mut restored = ByDate::new();
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
        tracing::info!(days, "日次集計を読み戻しました");
    }

    /// 変わった日だけをディスクへ落とす。変わっていなければ何もしない。
    ///
    /// 書くのは自分のファイルだけ。日付ごとに分けて書くので、日を跨いだ直後に
    /// 残っている前日分もそのまま正しい先へ行く。
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
            if let Err(e) = write_day(&self.path_of(date), &day) {
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

    /// 一定の間隔で落とし続ける。
    ///
    /// 間隔を空けるのは、1 リクエストごとに書くのが無駄だから (kawaz 裁定)。
    /// 落とし損なった分は次の周回で書き直されるので、ここでは止まらない。
    pub async fn keep_flushing(&self, every: std::time::Duration) {
        let mut ticker = tokio::time::interval(every);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = self.flush() {
                tracing::warn!(%e, "日次集計を保存できません");
            }
        }
    }

    /// 全 writer のファイルとメモリの分を合わせた全体像。
    ///
    /// 落とす前の分もここに出る。閲覧が「さっき使った分が出ない」にならない
    /// ようにするため。ただし**他の writer がまだ落としていない分は見えない**
    /// (向こうの保存間隔だけ遅れて現れる)。
    pub fn report(&self, days: usize, now: i64) -> Report {
        let mine = self.in_memory();
        // メモリに載っている日は、自分のファイルより新しい。その日だけ
        // 自分のファイルを読み飛ばす (両方足すと二重に数える)。読み戻しの
        // 範囲外の過去日は、メモリに無いのでファイルから読む。
        let superseded: BTreeSet<&str> = mine.keys().map(String::as_str).collect();

        let mut merged = self.on_disk(&superseded);
        for (date, day) in mine {
            merge_day(merged.entry(date).or_default(), day);
        }

        // 直近 N 日に絞る。日付は文字列だが `YYYY-MM-DD` は辞書順が日付順。
        // 上限で抑えてから秒に直す (抑えないと桁あふれで起点が未来に回る)。
        if days > 0 {
            let back = (days.min(MAX_DAYS) as i64 - 1).saturating_mul(86_400);
            let from = local_date(now.saturating_sub(back));
            merged.retain(|date, _| date.as_str() >= from.as_str());
        }
        Report {
            generated_at: now,
            generated_at_iso: crate::credential::time::format_rfc3339(now),
            days: merged,
        }
    }

    /// ディスクにある分を日付ごとに合わせる。
    ///
    /// `superseded` に挙げた日については、自分のファイルを読み飛ばす
    /// (メモリの方が新しい)。他の writer のファイルは常に読む。
    fn on_disk(&self, superseded: &BTreeSet<&str>) -> ByDate {
        let mut merged = ByDate::new();
        let own = self.own_suffix();
        for (date, path) in self.day_files() {
            let is_own = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&own));
            if is_own && superseded.contains(date.as_str()) {
                continue;
            }
            let Ok(day) = read_day(&path) else {
                tracing::warn!(path = %path.display(), "日次集計を読めません");
                continue;
            };
            merge_day(merged.entry(date).or_default(), day);
        }
        merged
    }

    /// 自分が書いたその日のファイル。無い / 読めない / 空なら `None`。
    fn read_own_day(&self, date: &str) -> Option<ByCredential> {
        let path = self.path_of(date);
        if !path.exists() {
            return None;
        }
        match read_day(&path) {
            Ok(day) if !day.is_empty() => Some(day),
            Ok(_) => None,
            Err(e) => {
                // 読めない 1 日分で起動や集計を止めない。
                tracing::warn!(path = %path.display(), %e, "日次集計を読めません");
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

    /// 前回の保存が途中で落ちて残った自分の一時ファイルを消す。
    ///
    /// rename まで進めなかった残骸は誰も読まないが、置き場に溜まり続ける。
    /// 消すのは**自分の名前が付いたものだけ** — 他の writer の一時ファイルは
    /// 今まさに書いている途中かもしれない。
    fn sweep_temporaries(&self) {
        let prefix = format!("{}.json.tmp.", self.writer);
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // `2026-07-30.8402.json.tmp.<pid>.<連番>` の後半で見分ける。
            if !name.contains(&prefix) {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => tracing::info!(path = %path.display(), "書き損じを片付けました"),
                Err(e) => tracing::warn!(path = %path.display(), %e, "書き損じを消せません"),
            }
        }
    }

    fn own_suffix(&self) -> String {
        format!(".{}.json", self.writer)
    }

    fn path_of(&self, date: &str) -> PathBuf {
        self.dir.join(format!("{date}.{}.json", self.writer))
    }
}

/// ファイル名から日付を取り出す。`2026-07-30.8402.json` → `2026-07-30`。
///
/// 形が合わないものは無視する。置き場に紛れ込んだ別のファイルを日付として
/// 読むと、ありえない日付が一覧に出る。
fn date_of_file(name: &str) -> Option<String> {
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

/// 書き手の名前をファイル名に使える形にする。
///
/// `127.0.0.1:8402` のような待ち受け先がそのまま来る。`.` はファイル名の
/// 区切りに使っているので、混ざると日付の切り出しが狂う。
fn sanitize_writer(writer: &str) -> String {
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

/// 1 日分を足し込む。ファイル同士・ファイルとメモリを合わせるときに使う。
fn merge_day(into: &mut ByCredential, day: ByCredential) {
    for (credential, models) in day {
        let into = into.entry(credential).or_default();
        for (model, counters) in models {
            into.entry(model).or_default().merge(&counters);
        }
    }
}

fn read_day(path: &Path) -> std::io::Result<ByCredential> {
    let raw = std::fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(std::io::Error::other)
}

/// 一時ファイルに振る通し番号。
///
/// pid だけでは、同じプロセスの 2 者が同じ名前を掴みうる。書く側は
/// [`Stats::writing`] で 1 人に絞ってあるが、名前も重ならないようにして
/// 二重に塞ぐ (排他が緩んだときに壊れ方が静かになるのを避ける)。
static NEXT_TEMPORARY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 1 日分を書く。
///
/// 一時ファイルに書いてから rename する。読む側 (CLI / 別プロセス) が途中の
/// 状態を読まないようにするため。名前に pid と通し番号を混ぜるのは、同じ置き場を
/// 使う別プロセス・同じプロセスの別の書き手と一時ファイルを取り違えないため
/// (DR-0010 と同じ理由)。
fn write_day(path: &Path, day: &ByCredential) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::Ordering;

    let seq = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{seq}", std::process::id()));
    let json = serde_json::to_vec_pretty(day).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        // rename する前にディスクへ落とす。省くと、クラッシュ時に「rename は
        // 済んだが中身が空」のファイルが残りうる。
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// SSE の 1 イベントをどこまで抱えるか。
///
/// 行の途中でチャンクが切れるので行が揃うまで持ち、さらに 1 イベントが複数の
/// `data:` 行に割れうるのでイベントが閉じるまで持つ。**書きかけの行と、その
/// イベントで溜めた分の合計**をこの上限で見る。壊れた相手 (改行も空行も返さない
/// upstream) にメモリを食い潰されないため。実際の `message_start` は 1KB 前後
/// なので、これでも桁が違う。
const MAX_SSE_EVENT: usize = 256 * 1024;

/// ストリームでない応答を、集計のためにどこまで抱えるか。
///
/// クライアントへはそのまま流れる。抱えるのは覗くための控えだけ。
const MAX_JSON_BODY: usize = 4 * 1024 * 1024;

/// 応答の本文を覗いて、使用量を集計に送る。
///
/// 流れるバイト列は変えない。集計できない応答 (2xx でない / usage を載せない
/// content-type) は包まずに返す — 覗く相手がいないのに層を重ねる意味がない。
///
/// `at` は観測時刻。日付をこれで決めるので、応答が始まった時刻を渡す
/// (生成が日を跨いで終わっても、始めた日に付ける)。
pub fn tap(
    body: crate::backend::anthropic::forward::BodyStream,
    stats: std::sync::Arc<Stats>,
    at: i64,
    status: u16,
    content_type: Option<&str>,
    credential: Option<&str>,
    model: &str,
) -> crate::backend::anthropic::forward::BodyStream {
    // エラーの本文に usage は載らない。読んでも取れないものに層を重ねない。
    if status / 100 != 2 {
        return body;
    }
    let Some(mode) = Mode::of(content_type) else {
        return body;
    };

    futures_util::StreamExt::boxed(Tap {
        inner: body,
        mode,
        held: Vec::new(),
        event: Vec::new(),
        given_up: false,
        tokens: Tokens::default(),
        stats,
        at,
        credential: credential.map(str::to_owned),
        model: model.to_owned(),
    })
}

/// 本文のどの読み方をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// SSE。`data:` の行に usage が現れる。
    Sse,
    /// ひとまとまりの JSON。終端で `/usage` を読む。
    Json,
}

impl Mode {
    /// この content-type から usage を読めるか。
    ///
    /// 分からない形は読まない。中身を推測して読みに行くと、画像やバイナリを
    /// JSON として抱え込むことになる。
    fn of(content_type: Option<&str>) -> Option<Self> {
        // `application/json; charset=utf-8` のような付属物を落とす。
        let base = content_type?.split(';').next()?.trim().to_ascii_lowercase();
        match base.as_str() {
            "text/event-stream" => Some(Self::Sse),
            "application/json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// 覗きながら流すストリーム。
///
/// チャンクは触らずそのまま下流へ渡し、控えの側だけを進める。
struct Tap {
    inner: crate::backend::anthropic::forward::BodyStream,
    mode: Mode,
    /// 行の途中 (SSE) / 本文の全部 (JSON) を溜める控え。
    held: Vec<u8>,
    /// 今のイベントで溜めた `data:` の中身 (SSE)。複数行なら改行で繋いである。
    event: Vec<u8>,
    /// 上限を超えたので、この応答の集計をやめた。
    given_up: bool,
    tokens: Tokens,
    stats: std::sync::Arc<Stats>,
    at: i64,
    credential: Option<String>,
    model: String,
}

impl Tap {
    /// 通り過ぎたチャンクを控えに取り込む。
    fn observe(&mut self, chunk: &[u8]) {
        if self.given_up {
            return;
        }
        match self.mode {
            Mode::Sse => self.observe_sse(chunk),
            Mode::Json => {
                if self.held.len() + chunk.len() > MAX_JSON_BODY {
                    self.give_up("応答が大きすぎます");
                    return;
                }
                self.held.extend_from_slice(chunk);
            }
        }
    }

    /// SSE を行に切り、イベントが閉じるところで中身を読む。
    ///
    /// チャンクの境目は行の途中に落ちる。揃った行だけを処理し、残りは次の
    /// チャンクまで持つ。
    fn observe_sse(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == b'\n' {
                let line = std::mem::take(&mut self.held);
                self.read_sse_line(&line);
                continue;
            }
            // 書きかけの行と、このイベントで溜めた分の合計で見る。
            if self.held.len() + self.event.len() >= MAX_SSE_EVENT {
                self.give_up("SSE の 1 イベントが長すぎます");
                return;
            }
            self.held.push(b);
        }
    }

    /// SSE の 1 行を処理する。
    ///
    /// 空行はイベントの終わり。`data:` の行は中身を溜めるだけで、読むのは
    /// イベントが閉じたとき — **1 つのイベントの data は複数行に割れてよく、
    /// その場合は改行で繋いだものが 1 つの中身**になる (SSE の仕様)。行ごとに
    /// 解こうとすると、そうやって割られた usage を黙って取りこぼす。
    fn read_sse_line(&mut self, line: &[u8]) {
        // 行末の `\r` は終端の一部 (CRLF で区切る upstream がある)。
        let line = line.strip_suffix(b"\r").unwrap_or(line);

        if line.is_empty() {
            self.finish_event();
            return;
        }
        let Some(payload) = line.strip_prefix(b"data:") else {
            // `event:` / `id:` / 注釈行は読まない。usage を載せるのは data だけ。
            return;
        };
        // コロンの直後の空白 1 つは区切りの一部で、中身には入らない。
        let payload = payload.strip_prefix(b" ").unwrap_or(payload);

        if !self.event.is_empty() {
            self.event.push(b'\n');
        }
        self.event.extend_from_slice(payload);
    }

    /// イベントが閉じた。溜めた中身から usage を読む。
    ///
    /// JSON として解くのは usage が載っているものだけ。イベントは 1 応答で
    /// 何十個も流れるので、全部解くと中継の脇で無駄に働くことになる。
    fn finish_event(&mut self) {
        let event = std::mem::take(&mut self.event);
        if !contains(&event, b"\"usage\"") {
            return;
        }
        let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&event) else {
            return;
        };
        // `message_start` は `/message/usage`、`message_delta` は `/usage` に
        // 載せる。イベント名で決め打ちせず、在る方を読む。
        for pointer in ["/message/usage", "/usage"] {
            if let Some(usage) = parsed.pointer(pointer) {
                self.tokens.absorb(usage);
            }
        }
    }

    /// 上限を超えた。この応答の集計は捨てて、素通しに徹する。
    fn give_up(&mut self, reason: &str) {
        self.given_up = true;
        self.held = Vec::new();
        self.event = Vec::new();
        self.tokens = Tokens::default();
        tracing::warn!(reason, model = %self.model, "使用量の集計をやめます");
    }
}

impl futures_util::Stream for Tap {
    type Item = crate::Result<bytes::Bytes>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;

        // 中身は Pin<Box<..>> なので、包んだ側を動かしても差し支えない。
        let this = self.get_mut();
        match futures_util::Stream::poll_next(this.inner.as_mut(), cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.observe(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            other => other,
        }
    }
}

impl Drop for Tap {
    /// 集計へ送るのはここだけ。
    ///
    /// 正常に終わった場合も、クライアントが去って途中で捨てられた場合も同じ道を
    /// 通る。Drop は 1 度しか走らないので、二重に数える形にならない。途中まで
    /// 流れた応答は、そこまでに読めた分が入る (`message_start` まで届いていれば
    /// input は分かる) — 中断した分を丸ごと捨てると、実際に消費した入力が
    /// 記録から消える。
    fn drop(&mut self) {
        if self.given_up {
            return;
        }
        match self.mode {
            // ストリームでない応答は、ここで初めて全体が揃う。
            Mode::Json => {
                if !self.held.is_empty()
                    && let Ok(body) = serde_json::from_slice::<serde_json::Value>(&self.held)
                    && let Some(usage) = body.pointer("/usage")
                {
                    self.tokens.absorb(usage);
                }
            }
            // 終端が空行で閉じられていなければ、最後のイベントが溜まったまま
            // 残る。書きかけの行も最後の 1 行として扱う。
            Mode::Sse => {
                let last = std::mem::take(&mut self.held);
                if !last.is_empty() {
                    self.read_sse_line(&last);
                }
                self.finish_event();
            }
        }
        self.stats.record(
            self.at,
            self.credential.as_deref(),
            &self.model,
            &self.tokens,
        );
    }
}

/// `needle` を含むか。
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 閲覧に出す形。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub generated_at: i64,
    pub generated_at_iso: String,
    /// 日付 → 認証情報 → モデル → 集計。
    pub days: ByDate,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-29T12:00:00Z
    const NOW: i64 = 1_785_326_400;

    fn tokens(input: u64, output: u64) -> Tokens {
        Tokens {
            input: Some(input),
            output: Some(output),
            ..Tokens::default()
        }
    }

    fn stats(dir: &Path) -> Stats {
        Stats::new(dir, "8402")
    }

    /// 同じ (日, 認証情報, モデル) は足し合わされる。
    #[test]
    fn the_same_key_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m", &tokens(10, 5));
        s.record(NOW, Some("a"), "m", &tokens(3, 1));

        let counts = s.in_memory();
        let day = counts.values().next().expect("1 日分");
        let c = &day["a"]["m"];
        assert_eq!(c.requests, 2, "本数も数える");
        assert_eq!(c.input_tokens, 13);
        assert_eq!(c.output_tokens, 6);
    }

    /// 認証情報とモデルは別々の行になる。
    #[test]
    fn credentials_and_models_are_kept_apart() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "haiku", &tokens(1, 1));
        s.record(NOW, Some("a"), "opus", &tokens(2, 2));
        s.record(NOW, Some("b"), "haiku", &tokens(4, 4));

        let counts = s.in_memory();
        let day = counts.values().next().unwrap();
        assert_eq!(day["a"].len(), 2, "同じ認証情報の 2 モデル");
        assert_eq!(day["a"]["opus"].input_tokens, 2);
        assert_eq!(day["b"]["haiku"].input_tokens, 4);
    }

    /// 認証情報を持たない経路も落とさず記録する。
    #[test]
    fn a_route_without_a_credential_is_still_counted() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, None, "m", &tokens(7, 3));

        let counts = s.in_memory();
        assert_eq!(
            counts.values().next().unwrap()[NO_CREDENTIAL]["m"].input_tokens,
            7
        );
    }

    /// usage が 1 つも載っていなければ記録しない。
    ///
    /// `count_tokens` のような応答で本数だけ増えると、使っていない日が
    /// 「使った日」に見える。
    #[test]
    fn a_response_without_usage_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m", &Tokens::default());

        assert!(s.in_memory().is_empty());
    }

    /// 日を跨いだら別の日付に積む。
    #[test]
    fn crossing_midnight_starts_a_new_day() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        // 地方時に依らず日付が変わる距離。
        s.record(NOW + 2 * 86_400, Some("a"), "m", &tokens(2, 2));

        let counts = s.in_memory();
        assert_eq!(counts.len(), 2, "2 日分に分かれる: {counts:?}");
        for day in counts.values() {
            assert_eq!(day["a"]["m"].requests, 1, "日を跨いで混ざらない");
        }
    }

    /// 落として読み戻すと、同じ数が返ってくる。
    #[test]
    fn a_flush_round_trips_through_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let before = {
            let s = stats(dir.path());
            s.record(NOW, Some("a"), "m", &tokens(10, 5));
            s.record(NOW, None, "m", &tokens(1, 2));
            s.flush().unwrap();
            s.in_memory()
        };

        // 再起動に相当する。
        let after = {
            let s = stats(dir.path());
            assert!(s.in_memory().is_empty(), "読み戻す前は空");
            s.restore();
            s.in_memory()
        };

        assert_eq!(after, before, "落とした分がそのまま戻る");
    }

    /// 読み戻した分に足し続けられる。
    ///
    /// ここが崩れると、再起動のたびに当日分が 0 から数え直しになり、次の
    /// 保存で前回までの分を消す。
    #[test]
    fn counting_continues_from_what_was_restored() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = stats(dir.path());
            s.record(NOW, Some("a"), "m", &tokens(10, 5));
            s.flush().unwrap();
        }

        let s = stats(dir.path());
        s.restore();
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let s = stats(dir.path());
        s.restore();
        let counts = s.in_memory();
        let c = &counts.values().next().unwrap()["a"]["m"];
        assert_eq!(c.requests, 2, "前回の 1 本に足す");
        assert_eq!(c.input_tokens, 11);
    }

    /// 日ごとに別のファイルへ落ちる。
    #[test]
    fn each_day_gets_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        s.record(NOW + 2 * 86_400, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "{names:?}");
        for name in &names {
            assert!(name.ends_with(".8402.json"), "書き手の名前が入る: {name}");
        }
        assert!(
            !names.iter().any(|n| n.contains("tmp")),
            "一時ファイルを残さない: {names:?}"
        );
    }

    /// 変わっていなければ書き直さない。
    #[test]
    fn an_unchanged_aggregate_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let path = s.path_of(&local_date(NOW));
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();

        s.flush().unwrap();
        let second = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(first, second, "2 度目は触らない");
    }

    /// 別 writer のファイルも足して見せる。
    ///
    /// 8401 と 8402 が並走していても、片方から全体が見える。
    #[test]
    fn other_writers_are_merged_in() {
        let dir = tempfile::tempdir().unwrap();
        {
            let other = Stats::new(dir.path(), "8401");
            other.record(NOW, Some("a"), "m", &tokens(100, 50));
            other.flush().unwrap();
        }

        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(1, 2));

        let report = s.report(7, NOW);
        let c = &report.days[&local_date(NOW)]["a"]["m"];
        assert_eq!(c.requests, 2, "両方の writer を数える");
        assert_eq!(c.input_tokens, 101);
        assert_eq!(c.output_tokens, 52);
    }

    /// 落とした後でも二重に数えない。
    ///
    /// 自分のファイルとメモリの両方に同じ分が居るので、素直に足すと倍になる。
    #[test]
    fn flushed_counts_are_not_counted_twice() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(10, 5));
        s.flush().unwrap();

        let c = s.report(7, NOW).days[&local_date(NOW)]["a"]["m"];
        assert_eq!(c.requests, 1, "1 本のまま");
        assert_eq!(c.input_tokens, 10);
    }

    /// まだ落としていない分も閲覧に出る。
    #[test]
    fn unflushed_counts_show_up_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(3, 4));

        let c = s.report(7, NOW).days[&local_date(NOW)]["a"]["m"];
        assert_eq!(c.output_tokens, 4, "保存を待たずに見える");
    }

    /// `days` で直近だけに絞れる。
    #[test]
    fn the_report_can_be_narrowed_to_recent_days() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW - 10 * 86_400, Some("a"), "m", &tokens(1, 1));
        s.record(NOW, Some("a"), "m", &tokens(2, 2));

        let recent = s.report(7, NOW);
        assert_eq!(recent.days.len(), 1, "10 日前は入らない: {:?}", recent.days);
        assert!(recent.days.contains_key(&local_date(NOW)));

        let all = s.report(0, NOW);
        assert_eq!(all.days.len(), 2, "0 なら絞らない");
    }

    /// 今日を 1 日と数える。
    #[test]
    fn one_day_means_today() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW - 86_400, Some("a"), "m", &tokens(1, 1));
        s.record(NOW, Some("a"), "m", &tokens(1, 1));

        let today = s.report(1, NOW);
        assert_eq!(today.days.len(), 1, "{:?}", today.days);
        assert!(today.days.contains_key(&local_date(NOW)));
    }

    /// 置き場が無くても報告は返る (まだ 1 度も使っていない状態)。
    #[test]
    fn a_missing_directory_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = Stats::new(dir.path().join("not-yet"), "8402");
        assert!(s.report(7, NOW).days.is_empty());
        s.restore();
        assert!(s.in_memory().is_empty());
    }

    /// 置き場に紛れ込んだ別のファイルは無視する。
    #[test]
    fn unrelated_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "無関係").unwrap();
        std::fs::write(dir.path().join("summary.json"), "{}").unwrap();
        std::fs::write(dir.path().join("broken.8401.json"), "{ not json").unwrap();

        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(1, 1));

        let report = s.report(7, NOW);
        assert_eq!(report.days.len(), 1, "自分の分だけ: {:?}", report.days);
    }

    #[test]
    fn file_names_yield_their_date() {
        assert_eq!(
            date_of_file("2026-07-30.8402.json").as_deref(),
            Some("2026-07-30")
        );
        for bad in [
            "notes.txt",
            "summary.json",
            "2026-7-30.8402.json",
            "not-a-date.8402.json",
            "2026-07-30.8402.json.tmp.1",
        ] {
            assert_eq!(date_of_file(bad), None, "{bad}");
        }
    }

    /// 待ち受け先がそのまま来ても、ファイル名の区切りを壊さない。
    #[test]
    fn a_listen_address_becomes_a_usable_name() {
        let dir = tempfile::tempdir().unwrap();
        let s = Stats::new(dir.path(), "127.0.0.1:8402");
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(
            date_of_file(&names[0]).is_some(),
            "日付が読み出せる名前になる: {}",
            names[0]
        );
        // 書いたものを自分で読み戻せる。
        let s = Stats::new(dir.path(), "127.0.0.1:8402");
        s.restore();
        assert!(!s.in_memory().is_empty());
    }

    /// usage は累積で届くので、後から来た値で置き換える (実測 2026-07-30)。
    #[test]
    fn a_cumulative_usage_replaces_the_earlier_value() {
        let mut t = Tokens::default();
        // message_start
        t.absorb(&serde_json::json!({
            "input_tokens": 18,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "output_tokens": 1,
        }));
        // message_delta (累積の最終値)
        t.absorb(&serde_json::json!({
            "input_tokens": 18,
            "output_tokens": 16,
        }));

        assert_eq!(t.output, Some(16), "1 + 16 = 17 にしない");
        assert_eq!(t.input, Some(18), "同じ値を二重に足さない");
    }

    /// 後の usage に載っていないフィールドは、前に拾った値を保つ。
    #[test]
    fn fields_absent_from_a_later_usage_are_kept() {
        let mut t = Tokens::default();
        t.absorb(&serde_json::json!({
            "input_tokens": 20,
            "cache_read_input_tokens": 900,
        }));
        t.absorb(&serde_json::json!({"output_tokens": 7}));

        assert_eq!(t.input, Some(20));
        assert_eq!(t.cache_read, Some(900));
        assert_eq!(t.output, Some(7));
    }

    /// 数の付いていないフィールドは拾わない。
    #[test]
    fn non_numeric_usage_values_are_skipped() {
        let mut t = Tokens::default();
        t.absorb(&serde_json::json!({
            "input_tokens": "many",
            "output_tokens": null,
            "cache_read_input_tokens": 5,
        }));

        assert_eq!(t.input, None);
        assert_eq!(t.output, None);
        assert_eq!(t.cache_read, Some(5));
        assert!(!t.is_empty(), "読めた分があれば記録する");
    }

    // ---------- 保存の範囲と直列化 (レビュー指摘 A / B) ----------

    /// 読み戻しは直近だけでも、**過去日は閲覧に出る**。
    ///
    /// メモリに載っていない過去日は自分のファイルから読む。ここが抜けると、
    /// 再起動した瞬間に過去の記録が一覧から消える。
    #[test]
    fn past_days_are_still_visible_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let old = NOW - 10 * 86_400;
        {
            let s = stats(dir.path());
            s.record(old, Some("a"), "m", &tokens(100, 50));
            s.record(NOW, Some("a"), "m", &tokens(1, 2));
            s.flush().unwrap();
        }

        // 再起動。読み戻すのは直近 RESTORED_DAYS 日分だけ。
        let s = stats(dir.path());
        s.restore();
        assert!(
            !s.in_memory().contains_key(&local_date(old)),
            "10 日前はメモリに載せない"
        );

        let report = s.report(0, NOW);
        let c = &report.days[&local_date(old)]["a"]["m"];
        assert_eq!(c.requests, 1, "過去日はファイルから読む");
        assert_eq!(c.input_tokens, 100);
    }

    /// 読み戻しの範囲外の日へ積んでも、その日のファイルを消さない。
    ///
    /// メモリに無い日は、積む前にファイルを読んで土台にする。読まずに積むと
    /// 次の保存が過去日のファイルを上書きして消す。
    #[test]
    fn recording_into_an_unrestored_day_keeps_what_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let old = NOW - 10 * 86_400;
        {
            let s = stats(dir.path());
            s.record(old, Some("a"), "m", &tokens(100, 50));
            s.flush().unwrap();
        }

        let s = stats(dir.path());
        s.restore();
        // 時計が巻き戻った等で、載せていない日へ積む。
        s.record(old, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let s = stats(dir.path());
        let c = s.report(0, NOW).days[&local_date(old)]["a"]["m"];
        assert_eq!(c.requests, 2, "前からあった 1 本に足す");
        assert_eq!(c.input_tokens, 101, "上書きで消さない");
    }

    /// 変わった日だけを書き直す。
    ///
    /// 全体で 1 つの目印だと、1 件積むだけでメモリに載っている全日を
    /// 書き直すことになる (`an_unchanged_aggregate_is_not_rewritten` は
    /// 「1 つも変わっていない」場合しか見ていない)。
    #[test]
    fn only_the_changed_day_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        let yesterday = NOW - 86_400;

        s.record(yesterday, Some("a"), "m", &tokens(1, 1));
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let old_path = s.path_of(&local_date(yesterday));
        let before = std::fs::metadata(&old_path).unwrap().modified().unwrap();

        // 当日だけ積んで、もう一度落とす。
        s.record(NOW, Some("a"), "m", &tokens(2, 2));
        s.flush().unwrap();

        let after = std::fs::metadata(&old_path).unwrap().modified().unwrap();
        assert_eq!(before, after, "変わっていない日は触らない");

        // 当日側は更新されている。
        let today = read_day(&s.path_of(&local_date(NOW))).unwrap();
        assert_eq!(today["a"]["m"].requests, 2);
    }

    /// 保存に失敗した日は、次の保存で書き直される。
    #[test]
    fn a_failed_save_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        // 置き場と同じ名前のファイルを置いて、ディレクトリを作れなくする。
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();

        let s = Stats::new(&blocked, "8402");
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        assert!(s.flush().is_err(), "書けないので失敗する");

        // 目印が残っているので、書ける状態になれば落ちる。
        std::fs::remove_file(&blocked).unwrap();
        s.flush().unwrap();
        assert_eq!(
            read_day(&s.path_of(&local_date(NOW))).unwrap()["a"]["m"].requests,
            1
        );
    }

    /// 同時に保存しても、壊れたファイルにならない。
    ///
    /// 定期の保存と終了時の保存は重なりうる。同じ一時ファイルを取り合うと、
    /// 混ざった中身が rename されたり、片方が消したファイルをもう片方が
    /// rename しようとして失敗する。
    #[test]
    fn concurrent_saves_do_not_corrupt_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(stats(dir.path()));
        for i in 0..50 {
            s.record(NOW, Some("a"), "m", &tokens(i, i));
        }

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = std::sync::Arc::clone(&s);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        s.record(NOW, Some("a"), "m", &tokens(1, 1));
                        s.flush().unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // 読めること (= 途中の状態が rename されていない) を確かめる。
        let day = read_day(&s.path_of(&local_date(NOW))).unwrap();
        assert!(day["a"]["m"].requests > 0);

        // 一時ファイルを置き去りにしない。
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// 前回の書き損じ (自分の一時ファイル) は起動時に片付ける。
    #[test]
    fn leftover_temporaries_are_swept_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("2026-07-29.8402.json.tmp.1234.0");
        let theirs = dir.path().join("2026-07-29.8401.json.tmp.9999.0");
        std::fs::write(&mine, "{}").unwrap();
        std::fs::write(&theirs, "{}").unwrap();

        stats(dir.path()).restore();

        assert!(!mine.exists(), "自分の書き損じは消す");
        assert!(
            theirs.exists(),
            "他の writer の一時ファイルは触らない (書いている途中かもしれない)"
        );
    }

    /// 日数の指定が極端でも落ちない。
    #[test]
    fn an_extreme_day_count_does_not_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(1, 1));

        for days in [1, usize::MAX] {
            let report = s.report(days, NOW);
            assert!(
                report.days.contains_key(&local_date(NOW)),
                "days={days} で当日が消える"
            );
        }
    }

    // ---------- tap ----------

    use crate::backend::anthropic::forward::BodyStream;
    use futures_util::StreamExt as _;
    use std::sync::Arc;

    /// 実機で観測した SSE (2026-07-30、claude-haiku-4-5 / max_tokens 16)。
    ///
    /// 長い行は畳んであるが、usage の値と入れ物の形はそのまま。
    const REAL_SSE: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"model":"claude-haiku-4-5-20251001","#,
        r#""id":"msg_x","type":"message","role":"assistant","content":[],"usage":{"#,
        r#""input_tokens":18,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"#,
        "\"output_tokens\":1,\"service_tier\":\"standard\"}}   }\n",
        "\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0}\n",
        "\n",
        "event: ping\n",
        "data: {\"type\": \"ping\"}\n",
        "\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"one"}}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"#,
        r#""input_tokens":18,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"#,
        "\"output_tokens\":16}        }\n",
        "\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"   }\n\n",
    );

    fn stream_of(chunks: Vec<Vec<u8>>) -> BodyStream {
        futures_util::stream::iter(chunks.into_iter().map(|c| Ok(bytes::Bytes::from(c)))).boxed()
    }

    /// 1 文字ずつのチャンクに割る。行の途中で必ず切れる形。
    fn byte_by_byte(text: &str) -> Vec<Vec<u8>> {
        text.bytes().map(|b| vec![b]).collect()
    }

    /// tap を通して流し切り、集計に残ったものを返す。
    async fn drain(
        chunks: Vec<Vec<u8>>,
        status: u16,
        content_type: Option<&str>,
        dir: &Path,
    ) -> (String, ByDate) {
        let s = Arc::new(stats(dir));
        let mut out = Vec::new();
        {
            let mut tapped = tap(
                stream_of(chunks),
                Arc::clone(&s),
                NOW,
                status,
                content_type,
                Some("a"),
                "m",
            );
            while let Some(chunk) = tapped.next().await {
                out.extend_from_slice(&chunk.unwrap());
            }
        }
        (String::from_utf8(out).unwrap(), s.in_memory())
    }

    fn only_entry(counts: &ByDate) -> Counters {
        counts.values().next().expect("1 日分")["a"]["m"]
    }

    /// 実機の SSE から usage を拾う。累積の最終値が入る。
    #[tokio::test]
    async fn reads_usage_from_a_real_sse_stream() {
        let dir = tempfile::tempdir().unwrap();
        let (out, counts) = drain(
            vec![REAL_SSE.as_bytes().to_vec()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out, REAL_SSE, "1 バイトも変えない");
        let c = only_entry(&counts);
        assert_eq!(c.requests, 1);
        assert_eq!(c.input_tokens, 18);
        assert_eq!(c.output_tokens, 16, "message_start の 1 を足さない");
    }

    /// チャンクが行の途中で切れても取りこぼさない。
    ///
    /// 1 バイトずつ流すのは、境目の入りうる全ての位置を一度に試すため。
    #[tokio::test]
    async fn a_line_split_across_chunks_is_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let (out, counts) = drain(
            byte_by_byte(REAL_SSE),
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out, REAL_SSE, "1 バイトも変えない");
        let c = only_entry(&counts);
        assert_eq!(c.input_tokens, 18);
        assert_eq!(c.output_tokens, 16);
    }

    /// content-type に charset が付いていても読む。
    #[tokio::test]
    async fn a_content_type_with_parameters_is_understood() {
        let dir = tempfile::tempdir().unwrap();
        let (_, counts) = drain(
            vec![REAL_SSE.as_bytes().to_vec()],
            200,
            Some("text/event-stream; charset=utf-8"),
            dir.path(),
        )
        .await;

        assert_eq!(only_entry(&counts).output_tokens, 16);
    }

    /// ストリームでない応答は本文の `/usage` を読む。
    #[tokio::test]
    async fn reads_usage_from_a_plain_json_response() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"type":"message","content":[{"type":"text","text":"ok"}],
            "usage":{"input_tokens":11,"output_tokens":22,
            "cache_creation_input_tokens":3,"cache_read_input_tokens":4}}"#;

        let (out, counts) = drain(
            byte_by_byte(body),
            200,
            Some("application/json"),
            dir.path(),
        )
        .await;

        assert_eq!(out, body);
        let c = only_entry(&counts);
        assert_eq!(c.input_tokens, 11);
        assert_eq!(c.output_tokens, 22);
        assert_eq!(c.cache_creation_input_tokens, 3);
        assert_eq!(c.cache_read_input_tokens, 4);
    }

    /// usage を載せない応答は記録しない (`count_tokens` がこれ)。
    #[tokio::test]
    async fn a_response_without_usage_leaves_no_trace() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"input_tokens":42}"#;

        let (out, counts) = drain(
            vec![body.as_bytes().to_vec()],
            200,
            Some("application/json"),
            dir.path(),
        )
        .await;

        assert_eq!(out, body, "本文はそのまま返る");
        assert!(counts.is_empty(), "本数も増やさない: {counts:?}");
    }

    /// 2xx でない応答は集計しない。
    #[tokio::test]
    async fn an_error_response_is_not_counted() {
        let dir = tempfile::tempdir().unwrap();
        // 万一読まれたら気づけるよう、usage の形を持たせておく。
        let body = r#"{"type":"error","usage":{"input_tokens":99}}"#;

        let (out, counts) = drain(
            vec![body.as_bytes().to_vec()],
            429,
            Some("application/json"),
            dir.path(),
        )
        .await;

        assert_eq!(out, body);
        assert!(counts.is_empty(), "{counts:?}");
    }

    /// 知らない content-type は覗かない。
    #[tokio::test]
    async fn an_unknown_content_type_passes_through_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"usage":{"input_tokens":99}}"#;

        for content_type in [None, Some("text/plain"), Some("application/octet-stream")] {
            let (out, counts) = drain(
                vec![body.as_bytes().to_vec()],
                200,
                content_type,
                dir.path(),
            )
            .await;
            assert_eq!(out, body, "{content_type:?}");
            assert!(counts.is_empty(), "{content_type:?}: {counts:?}");
        }
    }

    /// 途中で捨てられても、そこまでに読めた分は残る。
    ///
    /// クライアントが去った場合がこれ。`message_start` まで届いていれば入力の
    /// 消費は確定しているので、記録から落とすと実際に使った分が消える。
    #[tokio::test]
    async fn an_aborted_stream_still_records_what_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(stats(dir.path()));

        // message_start まで流して、残りを読まずに捨てる。
        let head = REAL_SSE.split("event: content_block_start").next().unwrap();
        {
            let mut tapped = tap(
                stream_of(vec![head.as_bytes().to_vec()]),
                Arc::clone(&s),
                NOW,
                200,
                Some("text/event-stream"),
                Some("a"),
                "m",
            );
            let _ = tapped.next().await;
        }

        let counts = s.in_memory();
        let c = only_entry(&counts);
        assert_eq!(c.requests, 1, "中断も 1 本として数える");
        assert_eq!(c.input_tokens, 18, "message_start で分かった入力は残る");
        assert_eq!(c.output_tokens, 1, "そこまでに見えた出力だけ");
    }

    /// 1 度しか数えない。
    ///
    /// 流し切った後に捨てても、終端と Drop で二重に記録しない。
    #[tokio::test]
    async fn a_completed_stream_is_recorded_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let (_, counts) = drain(
            vec![REAL_SSE.as_bytes().to_vec()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(only_entry(&counts).requests, 1);
        assert_eq!(only_entry(&counts).input_tokens, 18, "2 倍になっていない");
    }

    /// 改行の来ない長い行は、抱え込まずに諦める。
    #[tokio::test]
    async fn an_endless_line_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let flood = vec![b'x'; MAX_SSE_EVENT + 1024];

        let (out, counts) = drain(
            vec![flood.clone()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out.len(), flood.len(), "本文は最後まで流す");
        assert!(counts.is_empty(), "その応答の集計は捨てる: {counts:?}");
    }

    /// 諦めた後も本文は流れ続ける。
    ///
    /// 集計を捨てることと、中継を止めることは別。
    #[tokio::test]
    async fn giving_up_does_not_stop_the_relay() {
        let dir = tempfile::tempdir().unwrap();
        let flood = vec![b'x'; MAX_SSE_EVENT + 1];
        let tail = REAL_SSE.as_bytes().to_vec();

        let (out, counts) = drain(
            vec![flood.clone(), tail.clone()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out.len(), flood.len() + tail.len(), "後続も流れる");
        assert!(counts.is_empty(), "諦めたまま拾い直さない: {counts:?}");
    }

    /// 大きすぎる JSON も抱え込まない。
    #[tokio::test]
    async fn an_oversized_json_body_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = vec![b'x'; 1024 * 1024];
        let chunks: Vec<Vec<u8>> = (0..5).map(|_| chunk.clone()).collect();
        let total: usize = chunks.iter().map(Vec::len).sum();

        let (out, counts) = drain(chunks, 200, Some("application/json"), dir.path()).await;

        assert_eq!(out.len(), total, "本文は最後まで流す");
        assert!(counts.is_empty(), "{counts:?}");
    }

    /// 壊れた JSON でも中継は無事。
    #[tokio::test]
    async fn a_broken_body_does_not_break_the_relay() {
        let dir = tempfile::tempdir().unwrap();
        let body = "{ not json";

        let (out, counts) = drain(
            vec![body.as_bytes().to_vec()],
            200,
            Some("application/json"),
            dir.path(),
        )
        .await;

        assert_eq!(out, body);
        assert!(counts.is_empty(), "読めなければ記録しない");
    }

    /// upstream が途切れても、そこまでの分は残り、誤りは下流へ伝わる。
    #[tokio::test]
    async fn a_failing_stream_keeps_what_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(stats(dir.path()));
        let head = REAL_SSE.split("event: content_block_start").next().unwrap();

        let broken: BodyStream = futures_util::stream::iter(vec![
            Ok(bytes::Bytes::from(head.as_bytes().to_vec())),
            Err(crate::Error::Config("応答の読み取りが途切れました".into())),
        ])
        .boxed();

        let mut saw_error = false;
        {
            let mut tapped = tap(
                broken,
                Arc::clone(&s),
                NOW,
                200,
                Some("text/event-stream"),
                Some("a"),
                "m",
            );
            while let Some(item) = tapped.next().await {
                if item.is_err() {
                    saw_error = true;
                }
            }
        }

        assert!(saw_error, "誤りを飲み込まない");
        assert_eq!(only_entry(&s.in_memory()).input_tokens, 18);
    }

    /// SSE の `data:` 以外の行は読まない。
    ///
    /// `event:` の行や注釈に usage という語が混ざっても数えない。
    #[tokio::test]
    async fn only_data_lines_are_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let noise = concat!(
            ": usage を含むコメント行\n",
            "event: usage\n",
            "id: {\"usage\":{\"input_tokens\":999}}\n",
            "\n",
        );

        let (out, counts) = drain(
            vec![noise.as_bytes().to_vec()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out, noise);
        assert!(counts.is_empty(), "{counts:?}");
    }

    /// `data:` の後の空白は在っても無くてもよい。
    #[tokio::test]
    async fn a_data_line_without_a_space_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let sse = "data:{\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n";

        let (_, counts) = drain(
            vec![sse.as_bytes().to_vec()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(only_entry(&counts).output_tokens, 5);
    }

    /// 1 つのイベントの data が複数行に割れていても読む。
    ///
    /// SSE では同じイベントの連続する `data:` 行を改行で繋いだものが 1 つの
    /// 中身。行ごとに JSON として解こうとすると、こう割られた usage を黙って
    /// 取りこぼす。
    #[tokio::test]
    async fn a_usage_split_across_data_lines_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let sse = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\n",
            "data:  \"usage\":{\"input_tokens\":30,\n",
            "data:  \"output_tokens\":40}}\n",
            "\n",
        );

        let (out, counts) = drain(
            byte_by_byte(sse),
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out, sse, "1 バイトも変えない");
        let c = only_entry(&counts);
        assert_eq!(c.input_tokens, 30, "割れた行を繋いで読む");
        assert_eq!(c.output_tokens, 40);
    }

    /// 別のイベントの data 同士は繋がない。
    ///
    /// 空行で区切られていれば別の中身。繋ぐと壊れた JSON になって、どちらの
    /// usage も読めなくなる。
    #[tokio::test]
    async fn data_from_different_events_is_not_joined() {
        let dir = tempfile::tempdir().unwrap();
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n",
            "\n",
        );

        let (_, counts) = drain(
            byte_by_byte(sse),
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        let c = only_entry(&counts);
        assert_eq!(c.input_tokens, 7, "前のイベントから");
        assert_eq!(c.output_tokens, 9, "後のイベントから");
    }

    /// 終端が空行で閉じられていなくても、最後のイベントを読む。
    #[tokio::test]
    async fn a_last_event_without_a_blank_line_is_still_read() {
        let dir = tempfile::tempdir().unwrap();
        // 空行も末尾の改行も無い。
        let sse = "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":11}}";

        let (out, counts) = drain(
            vec![sse.as_bytes().to_vec()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out, sse);
        assert_eq!(only_entry(&counts).output_tokens, 11);
    }

    /// 溜め込む量は、書きかけの行とイベントの合計で見る。
    ///
    /// 行ごとの上限だけだと、短い data 行を無限に並べられて上限をすり抜ける。
    #[tokio::test]
    async fn many_short_data_lines_still_hit_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        // 1 行あたり 8 バイト程度で、合計が上限を超えるまで並べる。
        let line = "data: xx\n";
        let count = MAX_SSE_EVENT / line.len() + 16;
        let sse = line.repeat(count);

        let (out, counts) = drain(
            vec![sse.as_bytes().to_vec()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out.len(), sse.len(), "本文は最後まで流す");
        assert!(counts.is_empty(), "溜め込まず諦める: {counts:?}");
    }

    /// CRLF で区切る upstream でも読める。
    #[tokio::test]
    async fn crlf_line_endings_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let sse = "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\r\n\r\n";

        let (out, counts) = drain(
            vec![sse.as_bytes().to_vec()],
            200,
            Some("text/event-stream"),
            dir.path(),
        )
        .await;

        assert_eq!(out, sse, "1 バイトも変えない");
        assert_eq!(only_entry(&counts).output_tokens, 9);
    }
}
