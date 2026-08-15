//! 使用量の日次集計 (DR-0011)。
//!
//! upstream は消費したトークン数を**応答の本文に**載せて返す。ヘッダ由来の
//! 利用状況 ([`crate::quota`]) が「上限にどれだけ近いか」の今この瞬間の値なのに
//! 対して、こちらは「いつ・どの認証情報で・どのモデルに・どれだけ使ったか」を
//! 日ごとに積む。軸が直交しているので別の口にしてある。
//!
//! ## 何を読むかは provider、どう積むかは core
//!
//! 本文の形も、トークンの区分の呼び名も upstream の方言なので、読むのは
//! provider の [`crate::provider::Metering`] が作る
//! [`UsageObserver`] (DR-0014 §4)。ここが持つのは**正規化済みの集計レコード**
//! — 日 × credential × モデルの集計キーと、[`TokenUsage`] の区分ごとの数。
//! 区分は provider が増やせるので、知らない区分もそのまま積む。
//!
//! ## 本文を読むのはここではない
//!
//! 応答本文を覗いて usage を抽出しつつ流すのは
//! [`crate::exchange::observe`] の責務 (DR-0014 §1)。ここが持つのは積んだ
//! 後の置き場 — 書き手 ([`Stats::record`]) と読み手 ([`Stats::report`])
//! だけで、本文にもストリームにも触れない。
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
use crate::metering::{PricingSource, TokenKind, TokenUsage, round_usd};
use crate::persist::{sanitize_writer, sweep_temporaries, write_atomically};

/// 認証情報を持たない経路 (relay 型) の記録先。
///
/// 集計から落とさないのは、gateway 越しに使った分が行ごと消えると合計が
/// 合わなくなるため。認証情報の名前と衝突しない語を使う。
pub const NO_CREDENTIAL: &str = "-";

/// 積み上がった数。
///
/// ディスクにもこの形で落ちる。トークン数は区分ごとの map で持ち、`requests` と
/// 並べて `{"requests": 1, "tokens": {"input": 18, ...}}` になる。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counters {
    pub requests: u64,
    /// 区分ごとのトークン数。provider 固有の区分もそのまま入る。
    #[serde(flatten)]
    pub tokens: TokenUsage,
}

impl Counters {
    /// 1 応答分を足す。
    fn add(&mut self, usage: &TokenUsage) {
        self.requests += 1;
        self.tokens.add_assign(usage);
    }

    /// 別の集計を足し込む。ファイルをまたいで合わせるときや、閲覧側が
    /// 小計を作るときに使う。
    pub fn merge(&mut self, other: &Self) {
        self.requests += other.requests;
        self.tokens.add_assign(&other.tokens);
    }
}

impl<'de> Deserialize<'de> for Counters {
    /// 新旧どちらの形でも読む。
    ///
    /// 正規形へ移る前 (DR-0011 初版) のファイルは、区分を 1 つの upstream の
    /// 呼び名のまま 4 つ、`requests` と平らに並べていた。読み込みでだけ受けて
    /// 正規区分へ写す —
    /// 書き戻すのは新しい形なので、1 度落とせばそのファイルは移行済みになる。
    /// 読めなくすると、その日の記録が閲覧から消える (日ごとのファイルが正本で、
    /// 作り直せない)。
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// ファイルに載りうる鍵を全部受ける器。欠けているものは既定値。
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Stored {
            requests: u64,
            tokens: BTreeMap<TokenKind, u64>,
            input_tokens: u64,
            output_tokens: u64,
            cache_creation_input_tokens: u64,
            cache_read_input_tokens: u64,
        }

        let stored = Stored::deserialize(deserializer)?;
        let mut tokens = TokenUsage {
            tokens: stored.tokens,
        };
        for (count, kind) in [
            (stored.input_tokens, TokenKind::INPUT_NAME),
            (stored.output_tokens, TokenKind::OUTPUT_NAME),
            (
                stored.cache_creation_input_tokens,
                TokenKind::INPUT_CACHE_CREATION_NAME,
            ),
            (
                stored.cache_read_input_tokens,
                TokenKind::INPUT_CACHE_READ_NAME,
            ),
        ] {
            // 0 は区分ごと置かない。旧形式は使っていない区分にも 0 を書いて
            // いたので、そのまま写すと使っていない欄が並ぶ。
            if count > 0 {
                *tokens.tokens.entry(TokenKind::new(kind)).or_default() += count;
            }
        }
        Ok(Self {
            requests: stored.requests,
            tokens,
        })
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
/// 書き込みは本文観測の終わり ([`crate::exchange::observe`] の後始末) から
/// 呼ばれる。await できない場所なので
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
    pub fn record(&self, at: i64, credential: Option<&str>, model: &str, usage: &TokenUsage) {
        if usage.is_empty() {
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
            .add(usage);
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
    /// `now` を引数で受けるのは、読み戻す「当日」を試験から固定するため
    /// (実時計に縛ると、固定時刻で積んだデータが日付の進みで範囲外になる)。
    pub fn restore(&self, now: i64) {
        sweep_temporaries(&self.dir, &self.writer);

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
        tracing::info!(days, "loaded daily totals from disk");
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

    /// 全 writer のファイルとメモリの分を合わせた全体像。
    ///
    /// 落とす前の分もここに出る。閲覧が「さっき使った分が出ない」にならない
    /// ようにするため。ただし**他の writer がまだ落としていない分は見えない**
    /// (向こうの保存間隔だけ遅れて現れる)。
    ///
    /// `pricing` は 1 行ずつ単価を答える役。ここが単価表を持たないのは、
    /// いくら掛かるかを知っているのが答えた provider の側だから (DR-0014 §4)。
    pub fn report(&self, days: usize, now: i64, pricing: &dyn PricingSource) -> Report {
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
        let (days, total_usd) = price(merged, pricing);
        Report {
            generated_at: now,
            generated_at_iso: crate::credential::time::format_rfc3339(now),
            days,
            total_usd,
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
                tracing::warn!(path = %path.display(), "cannot read daily totals");
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

/// 閲覧に出す 1 行。トークン数に、その分の USD を添える。
///
/// 保存する形 ([`Counters`]) と分けているのは、**ファイルはトークン数だけ**を
/// 持つため (DR-0011)。単価は改定されるので、閲覧のたびに今の表で計算する。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub counters: Counters,
    /// 単価表に無いモデルは**欄ごと出さない**。0 でも「不明」でもなく、
    /// 言えることが無いという意味。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
}

/// 認証情報 → モデル → 行。
pub type EntriesByModel = BTreeMap<String, Entry>;
/// 1 日分。合計を内訳と同じ階層に置くと、認証情報名と衝突しうるので分ける。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Day {
    pub credentials: BTreeMap<String, EntriesByModel>,
    /// その日の合計。単価表にあるモデルの分だけを足す (無いモデルは素通し)。
    /// 1 つも出せなければ欄を出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_usd: Option<f64>,
}

/// 閲覧に出す形。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub generated_at: i64,
    pub generated_at_iso: String,
    /// 日付 → 1 日分。
    pub days: BTreeMap<String, Day>,
    /// 全期間の合計。日ごとの合計と同じく、出せる分だけを足す。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_usd: Option<f64>,
}

/// トークン数の器に単価を掛けて、閲覧に出す形へ組み替える。
///
/// 合計は「単価が分かる行の和」。分からない行を 0 として混ぜると、
/// 出ている数字が全体の額に見えてしまう。1 行も出せない日は合計も出さない。
///
/// 掛けるのは**単価を答えられた区分だけ**。provider が残した内訳や親区分の
/// 部分集合は、単価を持たないので合計に重ならない。
fn price(days: ByDate, pricing: &dyn PricingSource) -> (BTreeMap<String, Day>, Option<f64>) {
    let mut priced = BTreeMap::new();
    let mut grand: Option<f64> = None;

    for (date, creds) in days {
        let mut day = Day::default();
        for (credential, models) in creds {
            let mut entries = EntriesByModel::new();
            for (model, counters) in models {
                let usd = pricing
                    .pricing(&credential, &model)
                    .map(|rates| rates.cost(&counters.tokens));
                if let Some(usd) = usd {
                    day.total_usd = Some(day.total_usd.unwrap_or(0.0) + usd);
                }
                entries.insert(model, Entry { counters, usd });
            }
            day.credentials.insert(credential, entries);
        }
        day.total_usd = day.total_usd.map(round_usd);
        if let Some(total) = day.total_usd {
            grand = Some(grand.unwrap_or(0.0) + total);
        }
        priced.insert(date, day);
    }

    (priced, grand.map(round_usd))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-29T12:00:00Z
    const NOW: i64 = 1_785_326_400;

    fn tokens(input: u64, output: u64) -> TokenUsage {
        let mut usage = TokenUsage::default();
        usage.set(TokenKind::input(), input);
        usage.set(TokenKind::output(), output);
        usage
    }

    /// 積んだ区分の数。積んでいなければ 0。
    fn count(counters: &Counters, kind: TokenKind) -> u64 {
        counters.tokens.get(&kind).unwrap_or(0)
    }

    fn input_of(counters: &Counters) -> u64 {
        count(counters, TokenKind::input())
    }

    fn output_of(counters: &Counters) -> u64 {
        count(counters, TokenKind::output())
    }

    fn stats(dir: &Path) -> Stats {
        Stats::new(dir, "8402")
    }

    /// 試験用の単価。実際の表は provider の側にあるので、ここは形だけ真似る。
    ///
    /// `m-cheap` は input だけ $1、`m-rich` は 4 区分に別々の単価。それ以外の
    /// モデルは値付けできない。
    struct Rates;

    impl PricingSource for Rates {
        fn pricing(&self, _credential: &str, model: &str) -> Option<crate::metering::Pricing> {
            let rates: &[(TokenKind, f64)] = &match model {
                "m-cheap" => vec![(TokenKind::input(), 1.0)],
                "m-rich" => vec![
                    (TokenKind::input(), 5.0),
                    (TokenKind::output(), 25.0),
                    (TokenKind::input_cache_creation(), 6.25),
                    (TokenKind::input_cache_read(), 0.5),
                ],
                _ => return None,
            };
            Some(crate::metering::Pricing {
                rates: rates.iter().cloned().collect(),
            })
        }
    }

    /// 同じ (日, 認証情報, モデル) は足し合わされる。
    #[test]
    fn the_same_key_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m", &tokens(10, 5));
        s.record(NOW, Some("a"), "m", &tokens(3, 1));

        let counts = s.in_memory();
        let day = counts.values().next().expect("one day's worth");
        let c = &day["a"]["m"];
        assert_eq!(c.requests, 2, "the count is tallied too");
        assert_eq!(input_of(c), 13);
        assert_eq!(output_of(c), 6);
    }

    /// provider 固有の区分も、標準区分と同じように積まれる。
    ///
    /// 知らない区分を other へ潰すと、その provider の内訳が永久に失われる。
    #[test]
    fn a_provider_specific_kind_is_kept_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        let mut usage = tokens(10, 5);
        usage.set(TokenKind::output_reasoning(), 4);
        usage.set("provider.batch_prediction", 3);
        s.record(NOW, Some("a"), "m", &usage);
        s.record(NOW, Some("a"), "m", &usage);

        let counts = s.in_memory();
        let c = &counts.values().next().unwrap()["a"]["m"];
        assert_eq!(count(c, TokenKind::output_reasoning()), 8);
        assert_eq!(count(c, TokenKind::new("provider.batch_prediction")), 6);
    }

    // ---------- コスト換算 (DR-0011 の「表示側で足す」) ----------

    /// 単価表にあるモデルには `usd` が付き、無いモデルには付かない。
    #[test]
    fn known_models_are_priced_and_unknown_ones_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        // haiku-4-5 は input $1 / 100 万トークン。
        s.record(NOW, Some("a"), "m-cheap", &tokens(1_000_000, 0));
        s.record(NOW, Some("a"), "who-knows", &tokens(1_000_000, 0));

        let day = &s.report(7, NOW, &Rates).days[&local_date(NOW)];
        let models = &day.credentials["a"];
        assert_eq!(models["m-cheap"].usd, Some(1.0));
        assert_eq!(models["who-knows"].usd, None, "does not emit a guessed amount");
        // トークン数はどちらも残る。
        assert_eq!(input_of(&models["who-knows"].counters), 1_000_000);
    }

    /// 単価を聞くときは、行の鍵 (認証情報とモデル) をそのまま渡す。
    ///
    /// 経路によって値付けが違いうるので、モデル名だけでは足りない。
    #[test]
    fn the_pricing_source_is_asked_with_the_whole_key() {
        use std::sync::Mutex as StdMutex;

        /// 聞かれた鍵を控えるだけの役。値付けはしない。
        struct Asked(StdMutex<Vec<(String, String)>>);

        impl PricingSource for Asked {
            fn pricing(&self, credential: &str, model: &str) -> Option<crate::metering::Pricing> {
                self.0
                    .lock()
                    .unwrap()
                    .push((credential.to_owned(), model.to_owned()));
                None
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        // 認証情報を持たない経路の分も、同じ形で聞きに行く。
        s.record(NOW, None, "m", &tokens(1, 1));

        let asked = Asked(StdMutex::new(Vec::new()));
        s.report(7, NOW, &asked);

        assert_eq!(
            asked.0.into_inner().unwrap(),
            vec![
                (NO_CREDENTIAL.to_owned(), "m".to_owned()),
                ("a".to_owned(), "m".to_owned()),
            ]
        );
    }

    /// 日の合計も全体の合計も、**単価が分かる行だけ**の和。
    ///
    /// 分からない行を 0 として混ぜると、出ている額が全体に見えてしまう。
    #[test]
    fn totals_only_add_up_what_can_be_priced() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        // 同じ日に、単価の分かるモデル 2 つと分からないモデル 1 つ。
        s.record(NOW, Some("a"), "m-cheap", &tokens(1_000_000, 0)); // $1
        s.record(NOW, Some("b"), "m-rich", &tokens(1_000_000, 0)); // $5
        s.record(NOW, Some("b"), "who-knows", &tokens(9_000_000, 0)); // 不明

        let report = s.report(7, NOW, &Rates);
        assert_eq!(report.days[&local_date(NOW)].total_usd, Some(6.0));
        assert_eq!(report.total_usd, Some(6.0), "the total is the same sum");
    }

    /// 1 行も値付けできない日は、合計の欄ごと出さない。
    #[test]
    fn a_day_with_nothing_priceable_has_no_total() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "who-knows", &tokens(10, 5));

        let report = s.report(7, NOW, &Rates);
        assert_eq!(report.days[&local_date(NOW)].total_usd, None);
        assert_eq!(report.total_usd, None);
    }

    /// 単価表にある区分がそれぞれ別の単価で効く。
    #[test]
    fn each_token_kind_is_priced_separately() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        // opus-5: input $5 / output $25 / cache write $6.25 / cache read $0.5。
        let mut usage = tokens(1_000_000, 1_000_000);
        usage.set(TokenKind::input_cache_creation(), 1_000_000);
        usage.set(TokenKind::input_cache_read(), 1_000_000);
        s.record(NOW, Some("a"), "m-rich", &usage);

        let day = &s.report(7, NOW, &Rates).days[&local_date(NOW)];
        assert_eq!(day.credentials["a"]["m-rich"].usd, Some(36.75));
    }

    /// 単価を持たない内訳は、残るが課金されない。
    ///
    /// provider が親区分の部分集合 (キャッシュの内訳など) を足しても、単価表に
    /// 無い限り合計は動かない。ここが崩れると二重課金になる。
    #[test]
    fn an_unpriced_detail_is_kept_but_not_charged() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        let mut usage = TokenUsage::default();
        usage.set(TokenKind::input(), 1_000_000);
        // input の内訳 (単価表に無い)。
        usage.set("input.ephemeral_1h", 900_000);
        s.record(NOW, Some("a"), "m-rich", &usage);

        let entry = &s.report(7, NOW, &Rates).days[&local_date(NOW)].credentials["a"]["m-rich"];
        assert_eq!(entry.usd, Some(5.0), "only the input portion; breakdowns are not stacked on top");
        assert_eq!(
            count(&entry.counters, TokenKind::new("input.ephemeral_1h")),
            900_000,
            "non-billed breakdowns are still kept as observed values"
        );
    }

    /// JSON では、値付けできない行に `usd` の鍵自体が出ない。
    ///
    /// null や 0 を出すと、読む側が「0 ドルだった」と解釈できてしまう。
    #[test]
    fn an_unpriced_entry_has_no_usd_key_in_json() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m-rich", &tokens(10, 5));
        s.record(NOW, Some("a"), "who-knows", &tokens(10, 5));

        let json = serde_json::to_value(s.report(7, NOW, &Rates)).unwrap();
        let models = &json["days"][local_date(NOW)]["credentials"]["a"];
        assert!(models["m-rich"].get("usd").is_some());
        assert!(models["who-knows"].get("usd").is_none(), "{models}");
        // トークン数は区分ごとの表として出る。
        assert_eq!(models["who-knows"]["requests"], 1);
        assert_eq!(models["who-knows"]["tokens"]["input"], 10);
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
        assert_eq!(day["a"].len(), 2, "2 models under the same credential");
        assert_eq!(input_of(&day["a"]["opus"]), 2);
        assert_eq!(input_of(&day["b"]["haiku"]), 4);
    }

    /// 認証情報を持たない経路も落とさず記録する。
    #[test]
    fn a_route_without_a_credential_is_still_counted() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, None, "m", &tokens(7, 3));

        let counts = s.in_memory();
        assert_eq!(
            input_of(&counts.values().next().unwrap()[NO_CREDENTIAL]["m"]),
            7
        );
    }

    /// 区分が 1 つも無ければ記録しない。
    ///
    /// `count_tokens` のような応答で本数だけ増えると、使っていない日が
    /// 「使った日」に見える。
    #[test]
    fn a_response_without_usage_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m", &TokenUsage::default());

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
        assert_eq!(counts.len(), 2, "split into 2 days: {counts:?}");
        for day in counts.values() {
            assert_eq!(day["a"]["m"].requests, 1, "not mixed across days");
        }
    }

    /// 落として読み戻すと、同じ数が返ってくる。
    #[test]
    fn a_flush_round_trips_through_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let before = {
            let s = stats(dir.path());
            let mut usage = tokens(10, 5);
            usage.set("provider.batch_prediction", 2);
            s.record(NOW, Some("a"), "m", &usage);
            s.record(NOW, None, "m", &tokens(1, 2));
            s.flush().unwrap();
            s.in_memory()
        };

        // 再起動に相当する。
        let after = {
            let s = stats(dir.path());
            assert!(s.in_memory().is_empty(), "empty before reloading");
            s.restore(NOW);
            s.in_memory()
        };

        assert_eq!(after, before, "dropped fields come back as-is (including unknown categories)");
    }

    /// ディスクに落ちる形は `requests` + 区分ごとの `tokens`。
    #[test]
    fn the_saved_shape_is_the_normalized_record() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        let mut usage = tokens(18, 16);
        usage.set(TokenKind::input_cache_read(), 3);
        s.record(NOW, Some("a"), "m", &usage);
        s.flush().unwrap();

        let raw = std::fs::read_to_string(s.path_of(&local_date(NOW))).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            saved["a"]["m"],
            serde_json::json!({
                "requests": 1,
                "tokens": {"input": 18, "output": 16, "input.cache_read": 3},
            }),
            "{raw}"
        );
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
        s.restore(NOW);
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let s = stats(dir.path());
        s.restore(NOW);
        let counts = s.in_memory();
        let c = &counts.values().next().unwrap()["a"]["m"];
        assert_eq!(c.requests, 2, "adds to the previous one");
        assert_eq!(input_of(c), 11);
    }

    // ---------- 旧形式の読み込み (DR-0011 初版の 4 フィールド) ----------

    /// 旧形式で書かれた日次ファイルは、正規区分へ写して読む。
    #[test]
    fn a_legacy_day_file_is_read_into_normalized_kinds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(format!("{}.8402.json", local_date(NOW))),
            r#"{"a":{"m-rich":{"requests":2,"input_tokens":18,"output_tokens":16,
                "cache_creation_input_tokens":2,"cache_read_input_tokens":3}}}"#,
        )
        .unwrap();

        let s = stats(dir.path());
        s.restore(NOW);

        let counts = s.in_memory();
        let c = &counts[&local_date(NOW)]["a"]["m-rich"];
        assert_eq!(c.requests, 2);
        assert_eq!(input_of(c), 18);
        assert_eq!(output_of(c), 16);
        assert_eq!(count(c, TokenKind::input_cache_creation()), 2);
        assert_eq!(count(c, TokenKind::input_cache_read()), 3);
    }

    /// 旧形式のファイルに積み足しても、前からあった分は失われない。
    ///
    /// 読み戻して積み、落とすと新しい形で書き直される。
    #[test]
    fn a_legacy_day_file_can_be_added_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{}.8402.json", local_date(NOW)));
        std::fs::write(
            &path,
            r#"{"a":{"m":{"requests":1,"input_tokens":10,"output_tokens":5,
                "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        )
        .unwrap();

        let s = stats(dir.path());
        s.restore(NOW);
        s.record(NOW, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved["a"]["m"],
            serde_json::json!({"requests": 2, "tokens": {"input": 11, "output": 6}}),
            "an unused category is not reordered while staying at 0"
        );
    }

    /// 旧形式由来と新形式由来で、同じトークン数なら同じ USD になる。
    ///
    /// 移行の前後で請求額の見え方が変わらないことが、区分を写す規則の
    /// 正しさの担保になる。
    #[test]
    fn legacy_and_normalized_records_cost_the_same() {
        let dir = tempfile::tempdir().unwrap();
        // 別 writer のファイルとして旧形式を置く (自分のファイルはメモリが優先)。
        std::fs::write(
            dir.path().join(format!("{}.8401.json", local_date(NOW))),
            r#"{"legacy":{"m-rich":{"requests":1,"input_tokens":1000000,
                "output_tokens":1000000,"cache_creation_input_tokens":1000000,
                "cache_read_input_tokens":1000000}}}"#,
        )
        .unwrap();

        let s = stats(dir.path());
        let mut usage = tokens(1_000_000, 1_000_000);
        usage.set(TokenKind::input_cache_creation(), 1_000_000);
        usage.set(TokenKind::input_cache_read(), 1_000_000);
        s.record(NOW, Some("normalized"), "m-rich", &usage);

        let day = &s.report(7, NOW, &Rates).days[&local_date(NOW)];
        let legacy = day.credentials["legacy"]["m-rich"].usd;
        let normalized = day.credentials["normalized"]["m-rich"].usd;
        assert_eq!(legacy, Some(36.75));
        assert_eq!(legacy, normalized, "the conversion is unchanged for the legacy format too");
        assert_eq!(day.total_usd, Some(73.5), "the total is the sum of 2 rows");
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
            assert!(name.ends_with(".8402.json"), "includes the writer's name: {name}");
        }
        assert!(
            !names.iter().any(|n| n.contains("tmp")),
            "no temp file is left behind: {names:?}"
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
        assert_eq!(first, second, "the second time is untouched");
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

        let report = s.report(7, NOW, &Rates);
        let c = &report.days[&local_date(NOW)].credentials["a"]["m"].counters;
        assert_eq!(c.requests, 2, "counts both writers");
        assert_eq!(input_of(c), 101);
        assert_eq!(output_of(c), 52);
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

        let c = &s.report(7, NOW, &Rates).days[&local_date(NOW)].credentials["a"]["m"].counters;
        assert_eq!(c.requests, 1, "stays at one");
        assert_eq!(input_of(c), 10);
    }

    /// まだ落としていない分も閲覧に出る。
    #[test]
    fn unflushed_counts_show_up_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(3, 4));

        let c = &s.report(7, NOW, &Rates).days[&local_date(NOW)].credentials["a"]["m"].counters;
        assert_eq!(output_of(c), 4, "visible without waiting for a save");
    }

    /// `days` で直近だけに絞れる。
    #[test]
    fn the_report_can_be_narrowed_to_recent_days() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW - 10 * 86_400, Some("a"), "m", &tokens(1, 1));
        s.record(NOW, Some("a"), "m", &tokens(2, 2));

        let recent = s.report(7, NOW, &Rates);
        assert_eq!(recent.days.len(), 1, "10 days ago is excluded: {:?}", recent.days);
        assert!(recent.days.contains_key(&local_date(NOW)));

        let all = s.report(0, NOW, &Rates);
        assert_eq!(all.days.len(), 2, "0 means no filtering");
    }

    /// 今日を 1 日と数える。
    #[test]
    fn one_day_means_today() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW - 86_400, Some("a"), "m", &tokens(1, 1));
        s.record(NOW, Some("a"), "m", &tokens(1, 1));

        let today = s.report(1, NOW, &Rates);
        assert_eq!(today.days.len(), 1, "{:?}", today.days);
        assert!(today.days.contains_key(&local_date(NOW)));
    }

    /// 置き場が無くても報告は返る (まだ 1 度も使っていない状態)。
    #[test]
    fn a_missing_directory_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = Stats::new(dir.path().join("not-yet"), "8402");
        assert!(s.report(7, NOW, &Rates).days.is_empty());
        s.restore(NOW);
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

        let report = s.report(7, NOW, &Rates);
        assert_eq!(report.days.len(), 1, "only its own: {:?}", report.days);
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
            "becomes a name the date can be read from: {}",
            names[0]
        );
        // 書いたものを自分で読み戻せる。
        let s = Stats::new(dir.path(), "127.0.0.1:8402");
        s.restore(NOW);
        assert!(!s.in_memory().is_empty());
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
        s.restore(NOW);
        assert!(
            !s.in_memory().contains_key(&local_date(old)),
            "10 days ago is not loaded into memory"
        );

        let report = s.report(0, NOW, &Rates);
        let c = &report.days[&local_date(old)].credentials["a"]["m"].counters;
        assert_eq!(c.requests, 1, "a past day is read from the file");
        assert_eq!(input_of(c), 100);
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
        s.restore(NOW);
        // 時計が巻き戻った等で、載せていない日へ積む。
        s.record(old, Some("a"), "m", &tokens(1, 1));
        s.flush().unwrap();

        let s = stats(dir.path());
        let report = s.report(0, NOW, &Rates);
        let c = &report.days[&local_date(old)].credentials["a"]["m"].counters;
        assert_eq!(c.requests, 2, "adds to the one already there");
        assert_eq!(input_of(c), 101, "not erased by the overwrite");
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
        assert_eq!(before, after, "an unchanged day is untouched");

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
        assert!(s.flush().is_err(), "fails because it cannot write");

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

        stats(dir.path()).restore(NOW);

        assert!(!mine.exists(), "removes its own failed write");
        assert!(
            theirs.exists(),
            "does not touch another writer's temp file (it may still be in progress)"
        );
    }

    /// 日数の指定が極端でも落ちない。
    #[test]
    fn an_extreme_day_count_does_not_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", &tokens(1, 1));

        for days in [1, usize::MAX] {
            let report = s.report(days, NOW, &Rates);
            assert!(
                report.days.contains_key(&local_date(NOW)),
                "today disappears with days={days}"
            );
        }
    }
}
