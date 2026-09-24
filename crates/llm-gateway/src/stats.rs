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

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::metering::{PricingSource, TokenKind, TokenUsage, round_usd};

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

/// 出した側が分からなかった 1 本の行き先 (DR-0029)。
///
/// 素性を知らない頃に書かれたファイルもここへ寄せる。「見分けが付かなかった」
/// という意味は [`crate::provider::RequestOrigin::Unknown`] と同じなので、
/// 過去の分のために別の語を増やさない。
pub const UNKNOWN_ORIGIN: &str = "unknown";

/// 素性 → 集計 (DR-0029)。
///
/// ファイルには**モデルの下**にこの形で落ちる。素性を知らない頃のファイルは
/// モデルの下にいきなり集計が置いてあるので、読むときだけ
/// [`UNKNOWN_ORIGIN`] の 1 本にくるんで受ける。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ByOrigin(BTreeMap<String, Counters>);

impl std::ops::Deref for ByOrigin {
    type Target = BTreeMap<String, Counters>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ByOrigin {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'de> Deserialize<'de> for ByOrigin {
    /// 素性のある形と、素性の無い頃の形のどちらでも読む。
    ///
    /// 見分けるのは**鍵の名前**。集計の器が持つ鍵 ([`Counters`] の
    /// `requests` / `tokens` と旧形式の 4 区分) と素性の語 (`main` / `sub` /
    /// `keepalive` …) は重ならないので、中身を見れば形が決まる。読めなく
    /// すると、その日の記録が閲覧から消える (日ごとのファイルが正本で、
    /// 作り直せない)。
    ///
    /// Design rationale: 鍵を見てから中身の読み方を決めるので、いったん
    /// [`serde_json::Value`] へ受ける。形の判別を型に任せる書き方
    /// (`#[serde(untagged)]`) を採らないのは、[`Counters`] が全欄 `default` で
    /// 読めるため**素性の表がそのまま空の集計として通ってしまう**ため
    /// (中身が黙って 0 になる)。この置き場のファイルは JSON だけなので、
    /// self-describing な形に限られる不利は効かない。
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let raw = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        if raw.keys().any(|key| COUNTER_KEYS.contains(&key.as_str())) {
            let counters =
                Counters::deserialize(serde_json::Value::Object(raw)).map_err(D::Error::custom)?;
            return Ok(Self(BTreeMap::from([(
                UNKNOWN_ORIGIN.to_owned(),
                counters,
            )])));
        }
        let mut by_origin = BTreeMap::new();
        for (origin, value) in raw {
            by_origin.insert(
                origin,
                Counters::deserialize(value).map_err(D::Error::custom)?,
            );
        }
        Ok(Self(by_origin))
    }
}

/// 集計の器 ([`Counters`]) が持ちうる鍵。素性の語と重ならないので、
/// 1 つでも見えたら「素性の無い頃の形」と決まる。
const COUNTER_KEYS: [&str; 6] = [
    "requests",
    "tokens",
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
];

/// モデル名 → 素性 → 集計。
pub type ByModel = BTreeMap<String, ByOrigin>;
/// 認証情報 → モデル → 素性 → 集計。1 日分のファイルの中身がこの形。
pub type ByCredential = BTreeMap<String, ByModel>;
/// 日付 → 認証情報 → モデル → 素性 → 集計。閲覧に出す形。
pub type ByDate = BTreeMap<String, ByCredential>;

pub use gateway_core::stats::MAX_DAYS;

impl gateway_core::stats::Mergeable for Counters {
    fn merge(&mut self, other: &Self) {
        Counters::merge(self, other);
    }

    fn is_empty(&self) -> bool {
        self.requests == 0 && self.tokens.is_empty()
    }
}

impl gateway_core::stats::Mergeable for ByOrigin {
    fn merge(&mut self, other: &Self) {
        gateway_core::stats::Mergeable::merge(&mut self.0, &other.0);
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 日ごと・認証情報ごと・モデルごとに積む器。
///
/// 日の振り分けと、書き手ごとのファイルへの置き方・合わせ方は汎用層
/// ([`gateway_core::stats::Stats`]) が持つ。ここが持つのは集計の鍵と値付け。
///
/// 書き込みは本文観測の終わり ([`crate::exchange::observe`] の後始末) から
/// 呼ばれる。await できない場所なので、積むのは同期で済ませる。
pub struct Stats {
    inner: gateway_core::stats::Stats<ByCredential>,
}

impl Stats {
    /// 置き場と書き手の名前を決めて作る。
    ///
    /// 起動時に自分のファイルを読み戻すのは呼び出し側 ([`Self::restore`])。
    pub fn new(dir: impl Into<PathBuf>, writer: &str) -> Self {
        Self {
            inner: gateway_core::stats::Stats::new(dir, writer),
        }
    }

    /// 1 応答分を積む。
    ///
    /// `at_secs` はイベントを観測した時刻 (**unix 秒**)。日付はこの時刻の
    /// 地方時で決める。
    ///
    /// ミリ秒で持っている時刻 (知らせの `ts` 等、DR-0012) を渡すときは
    /// [`crate::credential::time::to_unix_secs`] を通すこと。1000 倍のまま
    /// 渡すと日付が 5 桁の年へ飛び、その 1 本が集計から迷子になる。
    ///
    /// `origin` は出した側の 1 語 ([`crate::provider::RequestOrigin::as_str`]、
    /// DR-0029)。gateway 自身の自送信 (`keepalive`) を後から分けて読むための
    /// 軸で、単価には関わらない (単価はモデルまでで決まる)。
    pub fn record(
        &self,
        at_secs: i64,
        credential: Option<&str>,
        model: &str,
        origin: &str,
        usage: &TokenUsage,
    ) {
        if usage.is_empty() {
            return;
        }
        let credential = credential.unwrap_or(NO_CREDENTIAL).to_owned();
        self.inner.add(at_secs, |day| {
            day.entry(credential)
                .or_default()
                .entry(model.to_owned())
                .or_default()
                .entry(origin.to_owned())
                .or_default()
                .add(usage);
        });
    }

    /// メモリに積んである分。
    pub fn in_memory(&self) -> ByDate {
        self.inner.in_memory()
    }

    /// 起動時に、自分が前回書いたファイルを読み戻す
    /// ([`gateway_core::stats::Stats::restore`])。
    pub fn restore(&self, now: i64) {
        self.inner.restore(now);
    }

    /// 変わった日だけをディスクへ落とす。変わっていなければ何もしない。
    pub fn flush(&self) -> std::io::Result<()> {
        self.inner.flush()
    }

    /// 全 writer のファイルとメモリの分を合わせた全体像。
    ///
    /// 読めなかった writer の分は欠けたまま出す (閲覧は best-effort、
    /// DR-0031 §2 (3))。
    ///
    /// `pricing` は 1 行ずつ単価を答える役。ここが単価表を持たないのは、
    /// いくら掛かるかを知っているのが答えた provider の側だから (DR-0014 §4)。
    pub fn report(&self, days: usize, now_ms: i64, pricing: &dyn PricingSource) -> Report {
        let now_secs = crate::credential::time::to_unix_secs(now_ms);
        let merged = self.inner.merged(days, now_secs).value;
        let (days, total_usd) = price(merged, pricing);
        Report {
            generated_at: now_ms,
            days,
            total_usd,
        }
    }

    #[cfg(test)]
    fn path_of(&self, date: &str) -> PathBuf {
        self.inner.path_of(date)
    }
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
    /// 出した側ごとの内訳 (DR-0029)。`main` / `sub` / `keepalive` …。
    ///
    /// この行が持つ数は内訳の和と一致する。畳んだ数を別に置くのは、よく見る
    /// のが「このモデルにいくら使ったか」で、素性は当たりを付けた後に見る
    /// ため (1 日の合計を内訳と別に置いてあるのと同じ形)。
    ///
    /// 素性の段は単価に関わらないので、内訳の `usd` は親と同じ単価で出る。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub origins: BTreeMap<String, Entry>,
    /// `input` の数がどちらの数え方か (DR-0029)。
    #[serde(default)]
    pub input_basis: InputBasis,
}

/// 閲覧に出す `input` の数え方 (DR-0029)。
///
/// ファイルには upstream の数え方のまま積む (cache 込みの総数を返す
/// upstream も、cache を含まない数を返す upstream もある)。閲覧に出すときに、単価表が宣言する内訳
/// ([`crate::metering::Pricing::exclusive`]) を引いて「cache でない入力」へ
/// 揃える。単価表に無いモデルは内訳が分からないので揃えられず、そのまま出す。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputBasis {
    /// cache を含まない入力に揃えてある。
    Fresh,
    /// upstream が言った数のまま。cache 分を含むかどうかは分からない。
    ///
    /// 既定をこちらにするのは、この欄を知らない gateway の報告が揃えて
    /// いない数を運んでくるため。
    #[default]
    AsRecorded,
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
    /// この報告を組んだ時刻 (Unix ミリ秒)。
    pub generated_at: i64,
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
            for (model, by_origin) in models {
                // 単価はモデルまでで決まる (DR-0029)。素性ごとに聞き直さない。
                let rates = pricing.pricing(&credential, &model);
                let mut counters = Counters::default();
                let mut usd: Option<f64> = None;
                let mut origins = BTreeMap::new();
                let input_basis = match rates {
                    Some(_) => InputBasis::Fresh,
                    None => InputBasis::AsRecorded,
                };
                for (origin, mut slice) in by_origin.0 {
                    // 金額は積んだままの数で出す。揃えるのはその後。
                    let slice_usd = rates.as_ref().map(|rates| rates.cost(&slice.tokens));
                    if let Some(slice_usd) = slice_usd {
                        usd = Some(usd.unwrap_or(0.0) + slice_usd);
                    }
                    if let Some(rates) = &rates {
                        to_fresh_input(&mut slice, rates);
                    }
                    counters.merge(&slice);
                    origins.insert(
                        origin,
                        Entry {
                            counters: slice,
                            usd: slice_usd,
                            origins: BTreeMap::new(),
                            input_basis,
                        },
                    );
                }
                if let Some(usd) = usd {
                    day.total_usd = Some(day.total_usd.unwrap_or(0.0) + usd);
                }
                entries.insert(
                    model,
                    Entry {
                        counters,
                        usd,
                        origins,
                        input_basis,
                    },
                );
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

/// `input` を cache でない入力に揃える (DR-0029)。
///
/// 引く相手は単価表が内訳として宣言した区分で、金額の計算
/// ([`crate::metering::Pricing::cost`]) と同じ宣言を使う。数え方の規則を
/// 2 か所に持たないため。
fn to_fresh_input(counters: &mut Counters, rates: &crate::metering::Pricing) {
    let input = TokenKind::input();
    if counters.tokens.get(&input).is_none() {
        return;
    }
    let fresh = rates.exclusive(&counters.tokens, &input);
    counters.tokens.set(input, fresh);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::time::local_date;
    use gateway_core::stats::{date_of_file, millisecond_date_of_file};
    use std::path::Path;

    fn read_day(path: &Path) -> std::io::Result<ByCredential> {
        gateway_core::stats::read_day(path)
    }

    /// 2026-07-29T12:00:00Z
    const NOW: i64 = 1_785_326_400;
    /// 同じ時刻を Unix ミリ秒で。報告が運ぶ時刻はこの数え方 (DR-0012)。
    const NOW_MS: i64 = NOW * 1000;

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

    /// 試験の既定の素性。ほとんどの試験は素性に関心が無いので、1 語に固定する。
    const MAIN: &str = "main";

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
                ..Default::default()
            })
        }
    }

    /// ミリ秒を秒として数えた日付のファイルは、読み戻しで本来の日へ寄る。
    ///
    /// 5 桁の年のファイルは日付として読めないので、置いておくと閲覧から
    /// 消えたまま溜まり続ける。寄せ先に既にある分へ足し込み、寄せ終えた
    /// ファイルは消す。
    #[test]
    fn restoring_absorbs_a_day_filed_under_a_millisecond_date() {
        let dir = tempfile::tempdir().unwrap();

        // 時刻をミリ秒のまま積んでいた頃に出来たファイル。
        let stale = stats(dir.path());
        stale.record(NOW_MS, Some("a"), "m", MAIN, &tokens(10, 5));
        stale.flush().unwrap();
        let broken = dir.path().join(format!("{}.8402.json", local_date(NOW_MS)));
        assert!(broken.exists(), "the millisecond-dated file is there");

        // 同じ日には、秒で積んだ分が既にある。
        let sound = stats(dir.path());
        sound.record(NOW, Some("a"), "m", MAIN, &tokens(1, 2));
        sound.record(NOW, Some("b"), "m", MAIN, &tokens(7, 7));
        sound.flush().unwrap();

        let reopened = stats(dir.path());
        reopened.restore(NOW);

        assert!(!broken.exists(), "the millisecond-dated file is gone");
        let counts = reopened.in_memory();
        assert_eq!(
            counts.keys().collect::<Vec<_>>(),
            vec![&local_date(NOW)],
            "everything landed on the day it was really sent"
        );
        let day = &counts[&local_date(NOW)];
        let c = &day["a"]["m"][MAIN];
        assert_eq!(c.requests, 2, "both requests are counted");
        assert_eq!(input_of(c), 11);
        assert_eq!(output_of(c), 7);
        assert_eq!(
            day["b"]["m"][MAIN].requests, 1,
            "the rest of the day is intact"
        );
    }

    /// 素性の正しい日次ファイルは寄せる対象ではない。
    #[test]
    fn only_a_five_digit_year_counts_as_a_millisecond_date() {
        assert_eq!(millisecond_date_of_file("2026-07-29.8402.json"), None);
        // 5 桁の年は数へ戻り、1000 で割ると元の日に帰る。
        let name = format!("{}.8402.json", local_date(NOW_MS));
        let millis = millisecond_date_of_file(&name).expect("{name} is a millisecond date");
        assert_eq!(local_date(millis.div_euclid(1000)), local_date(NOW));
        assert_eq!(millisecond_date_of_file(&format!("{name}.tmp.1.2")), None);
        assert_eq!(millisecond_date_of_file("notes.json"), None);
    }

    /// 同じ (日, 認証情報, モデル) は足し合わされる。
    #[test]
    fn the_same_key_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m", MAIN, &tokens(10, 5));
        s.record(NOW, Some("a"), "m", MAIN, &tokens(3, 1));

        let counts = s.in_memory();
        let day = counts.values().next().expect("one day's worth");
        let c = &day["a"]["m"][MAIN];
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
        s.record(NOW, Some("a"), "m", MAIN, &usage);
        s.record(NOW, Some("a"), "m", MAIN, &usage);

        let counts = s.in_memory();
        let c = &counts.values().next().unwrap()["a"]["m"][MAIN];
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
        s.record(NOW, Some("a"), "m-cheap", MAIN, &tokens(1_000_000, 0));
        s.record(NOW, Some("a"), "who-knows", MAIN, &tokens(1_000_000, 0));

        let day = &s.report(7, NOW_MS, &Rates).days[&local_date(NOW)];
        let models = &day.credentials["a"];
        assert_eq!(models["m-cheap"].usd, Some(1.0));
        assert_eq!(
            models["who-knows"].usd, None,
            "does not emit a guessed amount"
        );
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
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
        // 認証情報を持たない経路の分も、同じ形で聞きに行く。
        s.record(NOW, None, "m", MAIN, &tokens(1, 1));

        let asked = Asked(StdMutex::new(Vec::new()));
        s.report(7, NOW_MS, &asked);

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
        s.record(NOW, Some("a"), "m-cheap", MAIN, &tokens(1_000_000, 0)); // $1
        s.record(NOW, Some("b"), "m-rich", MAIN, &tokens(1_000_000, 0)); // $5
        s.record(NOW, Some("b"), "who-knows", MAIN, &tokens(9_000_000, 0)); // 不明

        let report = s.report(7, NOW_MS, &Rates);
        assert_eq!(report.days[&local_date(NOW)].total_usd, Some(6.0));
        assert_eq!(report.total_usd, Some(6.0), "the total is the same sum");
    }

    /// 1 行も値付けできない日は、合計の欄ごと出さない。
    #[test]
    fn a_day_with_nothing_priceable_has_no_total() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "who-knows", MAIN, &tokens(10, 5));

        let report = s.report(7, NOW_MS, &Rates);
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
        s.record(NOW, Some("a"), "m-rich", MAIN, &usage);

        let day = &s.report(7, NOW_MS, &Rates).days[&local_date(NOW)];
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
        usage.set("input.long_context", 900_000);
        s.record(NOW, Some("a"), "m-rich", MAIN, &usage);

        let entry = &s.report(7, NOW_MS, &Rates).days[&local_date(NOW)].credentials["a"]["m-rich"];
        assert_eq!(
            entry.usd,
            Some(5.0),
            "only the input portion; breakdowns are not stacked on top"
        );
        assert_eq!(
            count(&entry.counters, TokenKind::new("input.long_context")),
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

        s.record(NOW, Some("a"), "m-rich", MAIN, &tokens(10, 5));
        s.record(NOW, Some("a"), "who-knows", MAIN, &tokens(10, 5));

        let json = serde_json::to_value(s.report(7, NOW_MS, &Rates)).unwrap();
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

        s.record(NOW, Some("a"), "haiku", MAIN, &tokens(1, 1));
        s.record(NOW, Some("a"), "opus", MAIN, &tokens(2, 2));
        s.record(NOW, Some("b"), "haiku", MAIN, &tokens(4, 4));

        let counts = s.in_memory();
        let day = counts.values().next().unwrap();
        assert_eq!(day["a"].len(), 2, "2 models under the same credential");
        assert_eq!(input_of(&day["a"]["opus"][MAIN]), 2);
        assert_eq!(input_of(&day["b"]["haiku"][MAIN]), 4);
    }

    /// 認証情報を持たない経路も落とさず記録する。
    #[test]
    fn a_route_without_a_credential_is_still_counted() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, None, "m", MAIN, &tokens(7, 3));

        let counts = s.in_memory();
        assert_eq!(
            input_of(&counts.values().next().unwrap()[NO_CREDENTIAL]["m"][MAIN]),
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

        s.record(NOW, Some("a"), "m", MAIN, &TokenUsage::default());

        assert!(s.in_memory().is_empty());
    }

    /// 日を跨いだら別の日付に積む。
    #[test]
    fn crossing_midnight_starts_a_new_day() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
        // 地方時に依らず日付が変わる距離。
        s.record(NOW + 2 * 86_400, Some("a"), "m", MAIN, &tokens(2, 2));

        let counts = s.in_memory();
        assert_eq!(counts.len(), 2, "split into 2 days: {counts:?}");
        for day in counts.values() {
            assert_eq!(day["a"]["m"][MAIN].requests, 1, "not mixed across days");
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
            s.record(NOW, Some("a"), "m", MAIN, &usage);
            s.record(NOW, None, "m", MAIN, &tokens(1, 2));
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

        assert_eq!(
            after, before,
            "dropped fields come back as-is (including unknown categories)"
        );
    }

    /// ディスクに落ちる形は `requests` + 区分ごとの `tokens`。
    #[test]
    fn the_saved_shape_is_the_normalized_record() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        let mut usage = tokens(18, 16);
        usage.set(TokenKind::input_cache_read(), 3);
        s.record(NOW, Some("a"), "m", MAIN, &usage);
        s.flush().unwrap();

        let raw = std::fs::read_to_string(s.path_of(&local_date(NOW))).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            saved["a"]["m"],
            serde_json::json!({
                MAIN: {
                    "requests": 1,
                    "tokens": {"input": 18, "output": 16, "input.cache_read": 3},
                },
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
            s.record(NOW, Some("a"), "m", MAIN, &tokens(10, 5));
            s.flush().unwrap();
        }

        let s = stats(dir.path());
        s.restore(NOW);
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
        s.flush().unwrap();

        let s = stats(dir.path());
        s.restore(NOW);
        let counts = s.in_memory();
        let c = &counts.values().next().unwrap()["a"]["m"][MAIN];
        assert_eq!(c.requests, 2, "adds to the previous one");
        assert_eq!(input_of(c), 11);
    }

    // ---------- 出した側 (origin) の軸 (DR-0029) ----------

    /// 同じ認証情報・同じモデルでも、出した側が違えば別の行になる。
    ///
    /// gateway 自身の自送信 (`keepalive`) を、人が出した 1 本と混ぜない。
    #[test]
    fn the_same_model_is_split_by_who_sent_it() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "m", MAIN, &tokens(10, 5));
        s.record(NOW, Some("a"), "m", "keepalive", &tokens(1, 1));
        s.record(NOW, Some("a"), "m", "keepalive", &tokens(2, 2));

        let counts = s.in_memory();
        let by_origin = &counts[&local_date(NOW)]["a"]["m"];
        assert_eq!(by_origin[MAIN].requests, 1);
        assert_eq!(input_of(&by_origin[MAIN]), 10);
        assert_eq!(by_origin["keepalive"].requests, 2, "both pings are counted");
        assert_eq!(input_of(&by_origin["keepalive"]), 3);
    }

    /// 素性を知らない頃のファイル (正規形) は `unknown` に寄る。
    ///
    /// 集計の器の鍵と素性の語は重ならないので、どちらの形かは中身で決まる。
    #[test]
    fn a_day_file_without_origins_is_read_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(format!("{}.8402.json", local_date(NOW))),
            r#"{"a":{"m":{"requests":2,"tokens":{"input":18,"output":16}}}}"#,
        )
        .unwrap();

        let s = stats(dir.path());
        s.restore(NOW);

        let counts = s.in_memory();
        let by_origin = &counts[&local_date(NOW)]["a"]["m"];
        assert_eq!(
            by_origin.keys().collect::<Vec<_>>(),
            vec![UNKNOWN_ORIGIN],
            "the whole row lands under one name"
        );
        let c = &by_origin[UNKNOWN_ORIGIN];
        assert_eq!(c.requests, 2);
        assert_eq!(input_of(c), 18);
        assert_eq!(output_of(c), 16);
    }

    /// 閲覧では、モデルの行に素性ごとの内訳が付く。
    ///
    /// 行そのものは内訳の和で、単価はモデルまでで決まるので素性ごとの額も
    /// 同じ単価で出る。
    #[test]
    fn the_report_carries_the_origins_under_each_model() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        // m-cheap は input $1 / 100 万トークン。
        s.record(NOW, Some("a"), "m-cheap", MAIN, &tokens(3_000_000, 0));
        s.record(
            NOW,
            Some("a"),
            "m-cheap",
            "keepalive",
            &tokens(1_000_000, 0),
        );

        let day = &s.report(7, NOW_MS, &Rates).days[&local_date(NOW)];
        let entry = &day.credentials["a"]["m-cheap"];
        assert_eq!(entry.counters.requests, 2, "the row is the sum of both");
        assert_eq!(entry.usd, Some(4.0));
        assert_eq!(entry.origins[MAIN].usd, Some(3.0));
        assert_eq!(entry.origins["keepalive"].usd, Some(1.0));
        assert_eq!(input_of(&entry.origins["keepalive"].counters), 1_000_000);
        assert_eq!(
            day.total_usd,
            Some(4.0),
            "splitting by origin does not change the day's total"
        );
    }

    /// 単価表に無いモデルは、素性ごとの内訳にも額が付かない。
    #[test]
    fn an_unpriced_model_leaves_every_origin_without_an_amount() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        s.record(NOW, Some("a"), "who-knows", "keepalive", &tokens(10, 5));

        let day = &s.report(7, NOW_MS, &Rates).days[&local_date(NOW)];
        let entry = &day.credentials["a"]["who-knows"];
        assert_eq!(entry.usd, None);
        assert_eq!(entry.origins["keepalive"].usd, None);
        assert_eq!(
            entry.origins["keepalive"].counters.requests, 1,
            "the tokens are still there"
        );
    }

    /// 別々の writer が同じ素性へ積んだ分は、閲覧で足し合わされる。
    #[test]
    fn origins_are_merged_across_writers() {
        let dir = tempfile::tempdir().unwrap();
        {
            let other = Stats::new(dir.path(), "8401");
            other.record(NOW, Some("a"), "m", "keepalive", &tokens(100, 50));
            other.flush().unwrap();
        }
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", "keepalive", &tokens(1, 2));

        let report = s.report(7, NOW_MS, &Rates);
        let entry = &report.days[&local_date(NOW)].credentials["a"]["m"];
        assert_eq!(entry.origins["keepalive"].counters.requests, 2);
        assert_eq!(input_of(&entry.origins["keepalive"].counters), 101);
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
        let c = &counts[&local_date(NOW)]["a"]["m-rich"][UNKNOWN_ORIGIN];
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
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
        s.flush().unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved["a"]["m"],
            serde_json::json!({
                // 素性を知らない頃の分は unknown に寄り、新しい 1 本は素性の
                // 下に並ぶ (DR-0029)。足して消えない。
                UNKNOWN_ORIGIN: {"requests": 1, "tokens": {"input": 10, "output": 5}},
                MAIN: {"requests": 1, "tokens": {"input": 1, "output": 1}},
            }),
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
        s.record(NOW, Some("normalized"), "m-rich", MAIN, &usage);

        let day = &s.report(7, NOW_MS, &Rates).days[&local_date(NOW)];
        let legacy = day.credentials["legacy"]["m-rich"].usd;
        let normalized = day.credentials["normalized"]["m-rich"].usd;
        assert_eq!(legacy, Some(36.75));
        assert_eq!(
            legacy, normalized,
            "the conversion is unchanged for the legacy format too"
        );
        assert_eq!(day.total_usd, Some(73.5), "the total is the sum of 2 rows");
    }

    /// 日ごとに別のファイルへ落ちる。
    #[test]
    fn each_day_gets_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
        s.record(NOW + 2 * 86_400, Some("a"), "m", MAIN, &tokens(1, 1));
        s.flush().unwrap();

        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "{names:?}");
        for name in &names {
            assert!(
                name.ends_with(".8402.json"),
                "includes the writer's name: {name}"
            );
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
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
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
            other.record(NOW, Some("a"), "m", MAIN, &tokens(100, 50));
            other.flush().unwrap();
        }

        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 2));

        let report = s.report(7, NOW_MS, &Rates);
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
        s.record(NOW, Some("a"), "m", MAIN, &tokens(10, 5));
        s.flush().unwrap();

        let c = &s.report(7, NOW_MS, &Rates).days[&local_date(NOW)].credentials["a"]["m"].counters;
        assert_eq!(c.requests, 1, "stays at one");
        assert_eq!(input_of(c), 10);
    }

    /// まだ落としていない分も閲覧に出る。
    #[test]
    fn unflushed_counts_show_up_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW, Some("a"), "m", MAIN, &tokens(3, 4));

        let c = &s.report(7, NOW_MS, &Rates).days[&local_date(NOW)].credentials["a"]["m"].counters;
        assert_eq!(output_of(c), 4, "visible without waiting for a save");
    }

    /// `days` で直近だけに絞れる。
    #[test]
    fn the_report_can_be_narrowed_to_recent_days() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW - 10 * 86_400, Some("a"), "m", MAIN, &tokens(1, 1));
        s.record(NOW, Some("a"), "m", MAIN, &tokens(2, 2));

        let recent = s.report(7, NOW_MS, &Rates);
        assert_eq!(
            recent.days.len(),
            1,
            "10 days ago is excluded: {:?}",
            recent.days
        );
        assert!(recent.days.contains_key(&local_date(NOW)));

        let all = s.report(0, NOW_MS, &Rates);
        assert_eq!(all.days.len(), 2, "0 means no filtering");
    }

    /// 今日を 1 日と数える。
    #[test]
    fn one_day_means_today() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());
        s.record(NOW - 86_400, Some("a"), "m", MAIN, &tokens(1, 1));
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));

        let today = s.report(1, NOW_MS, &Rates);
        assert_eq!(today.days.len(), 1, "{:?}", today.days);
        assert!(today.days.contains_key(&local_date(NOW)));
    }

    /// 置き場が無くても報告は返る (まだ 1 度も使っていない状態)。
    #[test]
    fn a_missing_directory_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = Stats::new(dir.path().join("not-yet"), "8402");
        assert!(s.report(7, NOW_MS, &Rates).days.is_empty());
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
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));

        let report = s.report(7, NOW_MS, &Rates);
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
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
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
            s.record(old, Some("a"), "m", MAIN, &tokens(100, 50));
            s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 2));
            s.flush().unwrap();
        }

        // 再起動。読み戻すのは直近 RESTORED_DAYS 日分だけ。
        let s = stats(dir.path());
        s.restore(NOW);
        assert!(
            !s.in_memory().contains_key(&local_date(old)),
            "10 days ago is not loaded into memory"
        );

        let report = s.report(0, NOW_MS, &Rates);
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
            s.record(old, Some("a"), "m", MAIN, &tokens(100, 50));
            s.flush().unwrap();
        }

        let s = stats(dir.path());
        s.restore(NOW);
        // 時計が巻き戻った等で、載せていない日へ積む。
        s.record(old, Some("a"), "m", MAIN, &tokens(1, 1));
        s.flush().unwrap();

        let s = stats(dir.path());
        let report = s.report(0, NOW_MS, &Rates);
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

        s.record(yesterday, Some("a"), "m", MAIN, &tokens(1, 1));
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
        s.flush().unwrap();

        let old_path = s.path_of(&local_date(yesterday));
        let before = std::fs::metadata(&old_path).unwrap().modified().unwrap();

        // 当日だけ積んで、もう一度落とす。
        s.record(NOW, Some("a"), "m", MAIN, &tokens(2, 2));
        s.flush().unwrap();

        let after = std::fs::metadata(&old_path).unwrap().modified().unwrap();
        assert_eq!(before, after, "an unchanged day is untouched");

        // 当日側は更新されている。
        let today = read_day(&s.path_of(&local_date(NOW))).unwrap();
        assert_eq!(today["a"]["m"][MAIN].requests, 2);
    }

    /// 保存に失敗した日は、次の保存で書き直される。
    #[test]
    fn a_failed_save_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        // 置き場と同じ名前のファイルを置いて、ディレクトリを作れなくする。
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();

        let s = Stats::new(&blocked, "8402");
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
        assert!(s.flush().is_err(), "fails because it cannot write");

        // 目印が残っているので、書ける状態になれば落ちる。
        std::fs::remove_file(&blocked).unwrap();
        s.flush().unwrap();
        assert_eq!(
            read_day(&s.path_of(&local_date(NOW))).unwrap()["a"]["m"][MAIN].requests,
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
            s.record(NOW, Some("a"), "m", MAIN, &tokens(i, i));
        }

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = std::sync::Arc::clone(&s);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));
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
        assert!(day["a"]["m"][MAIN].requests > 0);

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
        s.record(NOW, Some("a"), "m", MAIN, &tokens(1, 1));

        for days in [1, usize::MAX] {
            let report = s.report(days, NOW_MS, &Rates);
            assert!(
                report.days.contains_key(&local_date(NOW)),
                "today disappears with days={days}"
            );
        }
    }

    /// 実際の単価表で値付けする役。数え方の揃え方は表の宣言に従う。
    struct Table;

    impl PricingSource for Table {
        fn pricing(&self, _credential: &str, model: &str) -> Option<crate::metering::Pricing> {
            crate::preset::pricing::for_model(model)
        }
    }

    /// 閲覧に出す `input` は cache でない入力に揃う (DR-0029)。
    ///
    /// 総数で積まれた行は内訳を引き、もともと cache を含まない行はそのまま。
    /// 単価表に無いモデルは揃えられないので、積んだ数のまま印を付ける。
    /// 金額は積んだ数から出すので、揃えても変わらない。
    #[test]
    fn the_input_is_reported_without_cached_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let s = stats(dir.path());

        // 総数で積む upstream の行: input 1,000 のうち cached 600 / write 100。
        let mut total = tokens(1_000, 50);
        total.set(TokenKind::input_cache_read(), 600);
        total.set(TokenKind::input_cache_creation(), 100);
        s.record(NOW, Some("o"), "gpt-6-sol", MAIN, &total);
        s.record(NOW, Some("o"), "gpt-6-sol", "sub", &total);
        // サブスク経路の行は cache write に単価が無いが、数としては引く。
        s.record(NOW, Some("o"), "gpt-5.4", MAIN, &total);
        // cache を含まない数で積む upstream の行。
        let mut fresh = tokens(1_000, 50);
        fresh.set(TokenKind::input_cache_read(), 600);
        fresh.set(TokenKind::input_cache_creation(), 100);
        s.record(NOW, Some("c"), "claude-opus-5", MAIN, &fresh);
        s.record(NOW, Some("x"), "who-knows", MAIN, &total);

        let day = &s.report(7, NOW_MS, &Table).days[&local_date(NOW)];
        let gpt = &day.credentials["o"]["gpt-6-sol"];
        assert_eq!(input_of(&gpt.counters), 2 * 300);
        assert_eq!(input_of(&gpt.origins[MAIN].counters), 300);
        assert_eq!(input_of(&gpt.origins["sub"].counters), 300);
        assert_eq!(gpt.input_basis, InputBasis::Fresh);
        assert_eq!(
            count(&gpt.counters, TokenKind::input_cache_read()),
            2 * 600,
            "the cache columns keep what was recorded"
        );
        let expected = crate::preset::pricing::for_model("gpt-6-sol")
            .unwrap()
            .cost(&total);
        assert_eq!(
            gpt.origins[MAIN].usd,
            Some(expected),
            "usd is priced on the recorded counts"
        );

        let sub = &day.credentials["o"]["gpt-5.4"];
        assert_eq!(input_of(&sub.counters), 300);

        let claude = &day.credentials["c"]["claude-opus-5"];
        assert_eq!(input_of(&claude.counters), 1_000);
        assert_eq!(claude.input_basis, InputBasis::Fresh);
        assert_eq!(
            claude.usd,
            crate::preset::pricing::for_model("claude-opus-5").map(|p| p.cost(&fresh))
        );

        let unknown = &day.credentials["x"]["who-knows"];
        assert_eq!(input_of(&unknown.counters), 1_000);
        assert_eq!(unknown.input_basis, InputBasis::AsRecorded);
        assert_eq!(unknown.origins[MAIN].input_basis, InputBasis::AsRecorded);

        // ファイルは積んだ数のまま。
        s.flush().unwrap();
        let raw = std::fs::read_to_string(s.path_of(&local_date(NOW))).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(raw["o"]["gpt-6-sol"][MAIN]["tokens"]["input"], 1_000);
    }

    /// 欄を持たない報告 (揃える前の gateway が返したもの) は、揃っていない
    /// 数として読む。
    #[test]
    fn an_entry_without_a_basis_reads_as_recorded() {
        let entry: Entry = serde_json::from_str(r#"{"requests":1,"tokens":{"input":10}}"#).unwrap();
        assert_eq!(entry.input_basis, InputBasis::AsRecorded);
    }
}
