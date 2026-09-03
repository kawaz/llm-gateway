//! 設定ファイル。
//!
//! 既定の置き場は `$XDG_CONFIG_HOME/llm-gateway/config.toml`。`--config` で
//! 別の場所を指せる。
//!
//! ```toml
//! [server]
//! listen = "127.0.0.1:11300"
//!
//! [store]
//! type = "file"
//! # dir 省略時は $XDG_STATE_HOME/llm-gateway/credentials
//!
//! [stats]
//! # dir 省略時は $XDG_STATE_HOME/llm-gateway/stats
//!
//! # 認証情報の中身 (token 等) はここに書かない。store に置いた
//! # <key>.json を type と結びつけるだけ。
//! [credentials.claude-personal]
//! type = "claude_oauth"
//!
//! [credentials.bedrock]
//! type = "bedrock_api_key"
//!
//! [routes.claude-personal]
//! provider = "anthropic"
//! credential = "claude-personal"
//!
//! [routes.bedrock]
//! provider = "anthropic"
//! credential = "bedrock"
//! url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"
//!
//! [routes.cpa]
//! provider = "anthropic"
//! url = "http://127.0.0.1:8317"
//!
//! # 何を公開し、どこへ流すかは namespace ごとに書く。`default` は
//! # `/v1/...` (namespace を書かないパス) が解決される先で、他と同じ書式。
//! # `auth_token` を書かないと誰も通れない。
//! [ns.default]
//! auth_token = "十分に長い乱数"
//!
//! [ns.default.filter]
//! exclude = ["claude-3-*"]
//!
//! # モデルごとに、使う認証情報を優先順に並べる。上から試す。
//! [[ns.default.routing]]
//! models = ["claude-fable-*"]
//! routes = ["bedrock", "claude-personal"]
//!
//! [[ns.default.routing]]
//! models = ["gpt-*"]
//! routes = ["cpa"]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

mod extends;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: Server,

    #[serde(default)]
    pub store: Store,

    #[serde(default)]
    pub stats: Stats,

    /// upstream service status の取得と観測。
    #[serde(default)]
    pub status: StatusConfig,

    /// 認証情報の宣言。キーが store 内のファイル名 (`<key>.json`) になる。
    ///
    /// **全 namespace で共有する。** 同じアカウントを namespace ごとに
    /// 持つと、token の更新が競合するうえ何度もログインが要る。
    #[serde(default)]
    pub credentials: BTreeMap<String, CredentialSpec>,

    /// upstream へ出る経路。認証情報と話す API をここで組み合わせる。
    #[serde(default)]
    pub routes: BTreeMap<String, RouteSpec>,

    /// upstream への問い合わせ設定。
    ///
    /// これも共有。同じアカウントに namespace の数だけ聞きに行っても、
    /// 返ってくる一覧は同じ。
    #[serde(default)]
    pub discovery: Discovery,

    /// 起きたことの送り先 (DR-0012)。書かなければ送らない。
    #[serde(default)]
    pub webhook: Webhook,

    /// 名前空間。`/ns-<名前>/v1/messages` で使い分ける。
    ///
    /// 何を隠すか・どう振り分けるか・短い名前をどうするかは、使う人ごとに
    /// 違う。そこだけ分ける。
    ///
    /// `default` も特別扱いしない。namespace を書かないパス (`/v1/...`) は
    /// `default` に解決されるので、`[ns.default]` を書かなければ `/v1/...` は
    /// 404 になる (DR-0006)。
    #[serde(default, rename = "ns")]
    pub namespaces: BTreeMap<String, Namespace>,
}

/// 1 つの名前空間。使う人ごとに変えたいものだけを持つ。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Namespace {
    /// 公開するモデルの絞り込み。
    ///
    /// upstream から取れる一覧をそのまま出すと古い世代まで並ぶので、
    /// ここで隠す。`claude-opus-4*` のように書ける。
    #[serde(default)]
    pub filter: Filter,

    /// モデルごとの経路。パターンで書ける。
    ///
    /// 上から順に照合し、最初に当たったものを使う。書かれていないモデルは
    /// `credentials` の宣言順に試す。
    #[serde(default)]
    pub routing: Vec<RoutingRule>,

    /// モデルと呼び出し元ごとの prompt cache 戦略。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache: Vec<CacheRule>,

    /// 短い名前。値はパターンで、当たるもののうち一番新しいものに向く。
    /// 別の短い名前を指してもよい。
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,

    /// この namespace を使うのに要るトークン。
    ///
    /// クライアントの `Authorization` をこれと突き合わせる。**書かなければ
    /// 誰でも通す** (DR-0006)。手前 (tailnet / リバースプロキシ) で境界を
    /// 引く運用では、ここで二重に認証を求める意味がないうえ、クライアントに
    /// トークンを持たせること自体が邪魔になる (Claude Code は
    /// `ANTHROPIC_AUTH_TOKEN` があるとサブスクとしての振る舞いをやめる)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,

    /// この namespace で使う経路。空なら全部。
    ///
    /// 面ごとに upstream を分けたいときに絞る。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,

    /// Messages API の思考表示方法を強制する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<ThinkingDisplay>,
}

/// Messages API の思考表示方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

impl ThinkingDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }
}

/// 公開するモデルの絞り込み。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// 隠すモデル。`claude-opus-4*` のようにパターンで書ける。
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// 同じ優先度で試す経路。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RouteGroup {
    /// 1 経路だけのグループ。
    One(RouteEntry),
    /// 7 日枠のリセットが近い順に試す同格の経路。
    Equal(Vec<RouteEntry>),
}

/// 配列なら同格グループ、それ以外は 1 経路。
///
/// Design rationale: `#[serde(untagged)]` で書けるが、それだと中で起きた
/// 失敗が全部「どの変種にも当てはまらない」に潰れる。設定の書き損じは人が
/// 直すものなので、`step` が読めないのか経路名が抜けているのかが分かる
/// 必要がある。形を先に見てから 1 つの変種へ落とす。
impl<'de> Deserialize<'de> for RouteGroup {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        if let toml::Value::Array(items) = value {
            return items
                .into_iter()
                .map(|item| RouteEntry::from_value(item).map_err(serde::de::Error::custom))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map(Self::Equal);
        }
        RouteEntry::from_value(value)
            .map(Self::One)
            .map_err(serde::de::Error::custom)
    }
}

impl RouteGroup {
    fn entries(&self) -> impl Iterator<Item = &RouteEntry> {
        match self {
            Self::One(entry) => std::slice::from_ref(entry).iter(),
            Self::Equal(entries) => entries.iter(),
        }
    }

    fn routes(&self) -> impl Iterator<Item = &str> {
        self.entries().map(RouteEntry::name)
    }
}

/// 経路 1 本の書き方。名前だけでも、属性を付けた table でもよい。
///
/// DR-0015 が「グループ位置に table も書ける形を後から**追加**できる」と
/// した枠 — 文字列と table は形が重ならないので、既存の書き方はそのまま
/// 読める。**同格グループの中でも同じように書ける** (DR-0019 §1)。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RouteEntry {
    /// 名前だけ。
    Named(String),
    /// 属性を付けた 1 経路。
    Attributed(Box<AttributedRoute>),
}

impl RouteEntry {
    /// 文字列なら名前だけ、table なら属性つき。
    fn from_value(value: toml::Value) -> Result<Self> {
        match value {
            toml::Value::String(name) => Ok(Self::Named(name)),
            toml::Value::Table(_) => value
                .try_into()
                .map(|route| Self::Attributed(Box::new(route)))
                .map_err(|e| Error::Config(format!("this route is not readable: {e}"))),
            other => Err(Error::Config(format!(
                "a route is written as a name or a table, but this is {}",
                other.type_str()
            ))),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Named(name) => name,
            Self::Attributed(route) => &route.route,
        }
    }

    fn pace_cap(&self) -> Option<PaceCap> {
        match self {
            Self::Named(_) => None,
            Self::Attributed(route) => route.pace_cap,
        }
    }
}

/// 属性を付けて書いた 1 経路。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttributedRoute {
    /// 経路の名前。
    pub route: String,
    /// 経過ぶんを超えて使わせない上限 (DR-0019)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pace_cap: Option<PaceCap>,
}

/// 経過した時間ぶんを超えて枠を使わせない上限 (DR-0019)。
///
/// 窓の頭から測った経過を `step` 刻みの階段にし、その段まで使ってよい割合を
/// 予算にする。予算を超えた経路は**次の段に上がるまで**候補から外れる。
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PaceCap {
    /// 経過ぶんのうち使ってよい割合。既定は `"100%"` (按分線ちょうど)。
    ///
    /// **按分線を超える値は書けない** (`0%`〜`100%`)。この上限は借りる側が
    /// 貸す側のペースに食い込まないためのもので、先食いを許す方向は目的に
    /// 反する。`"80%"` のように手前で締める方向だけ書ける。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<Share>,
    /// 予算が増える刻み。既定は窓長の 1/14 (7 日枠なら 12 時間)。
    ///
    /// 刻まずに経過そのものを使うと予算が連続的に増え、上限際の経路が
    /// 「わずかに許可されては即使い切る」を繰り返す。段にすると、次に開く
    /// 時刻を先に言える。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<WindowSpan>,
    /// 使い切りの繰り上げ窓に入ったら、階段を解放するか (既定 `false`)。
    ///
    /// `true` にすると、同じ規則の `spend_down_within` の窓に入った時点で
    /// 上限の判定をやめ、リセットまでに残りを使い切らせる。リセット曜日が
    /// 都合のよい credential (週末に回るもの等) で選ぶ。
    ///
    /// 既定の `false` では最終段の端数が解放されないまま蒸発する。それが
    /// **保護**にあたる — 貸し手が営業時間中に使うかもしれない枠を、借り手が
    /// 最後まで食い尽くさない (DR-0019 §7)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spend_down_release: bool,
}

/// `step` を書かなかったときの刻み (窓長に対する割合)。
///
/// 7 日枠で 12 時間。1 日 2 段なら、遅れを取り戻す粒度としても、無駄な
/// 締め出しを避ける粗さとしても実用的な範囲に収まる。
const DEFAULT_STEP: WindowSpan = WindowSpan::Ratio(Share(1.0 / 14.0));

/// 今この時点で使ってよい量と、それが次に増える時。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// 使ってよい割合 (使用率と同じ 0.0〜1.0 の尺度)。
    pub allowed: f64,
    /// 次に予算が増える時刻 (窓の頭からの経過秒)。
    pub next_step_at: i64,
}

impl PaceCap {
    /// 窓の頭から `elapsed` 秒の時点での予算。
    ///
    /// 予算 = (階段まで下ろした経過 / 窓長) × `ratio`。経過そのものではなく
    /// 段で測るので、同じ段にいる間は予算が動かない。
    pub fn budget(self, window_seconds: u64, elapsed: i64) -> Budget {
        let step = self.step.unwrap_or(DEFAULT_STEP).seconds_in(window_seconds);
        // 刻みが窓より細かく丸められて 0 になっても、段の概念は保つ。
        let step = step.max(1);
        let elapsed = elapsed.clamp(0, window_seconds as i64);
        let steps = elapsed / step;
        let ratio = self.ratio.unwrap_or_default().as_fraction();
        Budget {
            allowed: (steps * step) as f64 / window_seconds as f64 * ratio,
            next_step_at: (steps + 1).saturating_mul(step),
        }
    }
}

/// `"100%"` の形で書く割合。0 % 〜 100 % だけ。
///
/// 設定に出てくる割合はすべてこの書式にしてある。素の小数と混在すると、
/// 読むたびに単位を確かめることになる。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Share(f64);

impl Share {
    /// 0.0〜1.0 の小数として使う。
    pub fn as_fraction(self) -> f64 {
        self.0
    }
}

impl Default for Share {
    /// 書かなければ按分線ちょうど (`"100%"`)。
    fn default() -> Self {
        Self(1.0)
    }
}

impl std::str::FromStr for Share {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let text = text.trim();
        let percent: f64 = text
            .strip_suffix('%')
            .ok_or_else(|| Error::Config(format!("`{text}` is not a percentage such as `100%`")))?
            .trim()
            .parse()
            .map_err(|_| Error::Config(format!("`{text}` is not a percentage such as `100%`")))?;
        if !(0.0..=100.0).contains(&percent) {
            return Err(Error::Config(format!(
                "`{text}` is outside the 0%-100% range"
            )));
        }
        Ok(Self(percent / 100.0))
    }
}

impl std::fmt::Display for Share {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}%", self.0 * 100.0)
    }
}

impl<'de> Deserialize<'de> for Share {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for Share {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

/// prompt cache の扱い。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheStrategy {
    #[default]
    Passthrough,
    None,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
    Keepalive,
}

impl CacheStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::None => "none",
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
            Self::Keepalive => "keepalive",
        }
    }
}

/// 1 つの prompt cache 規則。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheRule {
    pub models: Vec<String>,
    #[serde(default)]
    pub main: CacheStrategy,
    #[serde(default)]
    pub sub: CacheStrategy,
    #[serde(
        default = "default_keepalive_horizon",
        with = "optional_human_duration",
        skip_serializing_if = "is_default_keepalive_horizon"
    )]
    pub keepalive_horizon: Option<Duration>,
}

fn default_keepalive_horizon() -> Option<Duration> {
    Some(Duration::from_secs(8 * 60 * 60))
}

fn is_default_keepalive_horizon(value: &Option<Duration>) -> bool {
    *value == default_keepalive_horizon()
}

/// 1 つの振り分け規則。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRule {
    /// 対象のモデル。パターンで書ける。
    pub models: Vec<String>,
    /// 使う経路を優先順に。内側の配列は同格の経路。
    pub routes: Vec<RouteGroup>,
    /// リセットがこれだけ手前まで迫った経路を先頭へ繰り上げる (DR-0018)。
    ///
    /// 書かなければ繰り上げない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_down_within: Option<WindowSpan>,
}

/// 枠の窓に対して測る幅。
///
/// 窓長に対する割合でも、絶対時間でも書ける。割合は窓長にスケールするので、
/// 周期の違う credential を 1 つの規則で扱える (DR-0018 §1)。使い切りの
/// 手前の幅 (`spend_down_within`) と、予算が増える刻み (`pace_cap.step`) が
/// 同じ「窓に対する幅」なので、書き方も 1 つにしてある。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowSpan {
    /// 窓長に対する割合。
    Ratio(Share),
    /// 窓長に依らない絶対の秒数。
    Absolute(u64),
}

impl WindowSpan {
    /// この窓長で測ったときの秒数。
    pub fn seconds_in(self, window_seconds: u64) -> i64 {
        let seconds = match self {
            Self::Ratio(ratio) => (window_seconds as f64 * ratio.as_fraction()) as u64,
            Self::Absolute(seconds) => seconds,
        };
        seconds.min(i64::MAX as u64) as i64
    }
}

impl std::str::FromStr for WindowSpan {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.ends_with('%') {
            return Ok(Self::Ratio(text.parse()?));
        }
        let duration = humantime::parse_duration(text).map_err(|_| {
            Error::Config(format!(
                "`{text}` is neither a percentage (`25%`) nor a duration (`40h`)"
            ))
        })?;
        Ok(Self::Absolute(duration.as_secs()))
    }
}

impl std::fmt::Display for WindowSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ratio(ratio) => write!(f, "{ratio}"),
            Self::Absolute(seconds) => {
                write!(
                    f,
                    "{}",
                    humantime::format_duration(Duration::from_secs(*seconds))
                )
            }
        }
    }
}

impl<'de> Deserialize<'de> for WindowSpan {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for WindowSpan {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    /// 一覧を取り直す間隔 (秒)。
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
    /// 認証情報の更新を見に行く間隔 (秒)。
    ///
    /// `llm-gateway login` で認証情報を入れ直しても、一覧を取り直すまでは
    /// 失効中に消えたモデルが戻らない。次の `refresh_secs` を待つと最大 1 時間
    /// 経路が欠けたままになるので、更新に気づいた時点で取り直す。
    #[serde(default = "default_watch_secs")]
    pub watch_secs: u64,
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            refresh_secs: default_refresh_secs(),
            watch_secs: default_watch_secs(),
        }
    }
}

fn default_refresh_secs() -> u64 {
    3600
}

fn default_watch_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// 待ち受け先。
    ///
    /// **この設定が指す面の住所**でもある。CLI (`usage` / `stats`) は、
    /// ここへ問い合わせる。
    #[serde(default = "default_listen")]
    pub listen: String,

    /// この設定では待ち受けない。
    ///
    /// `--config` を省いて CLI を使うための設定 (`config.toml`) を置きたい
    /// ことがある。共通部分を土台にして (DR-0013) `listen` だけ書いておけば、
    /// `llm-gateway usage` が問い合わせ先を知れる。ただしその設定で
    /// `serve` を始めると、既に動いている面と待ち受け先を取り合う。
    ///
    /// **`listen` の意味は変わらない** — 「どこで待つか」ではなく「どこに
    /// いるか」を書いた欄として、問い合わせ先の組み立てには使い続ける。
    #[serde(default)]
    pub disabled: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            disabled: false,
        }
    }
}

/// 既定の待ち受け先。
///
/// 11300 は小文字の `llm` の字形から 113、gateway らしく末尾を 00 にしたもの。
fn default_listen() -> String {
    "127.0.0.1:11300".to_owned()
}

/// 起きたことを送る先 (DR-0012)。
///
/// 見る側が繋ぎに来る形 (SSE) だと、待ち受けを複数並べたときに**掴んだ 1 つの
/// 面の分しか見えない**。こちらから送れば、面がいくつあっても同じ受け口に
/// 集まる。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    /// 受け口の根。ここに受け取る側が決めたパスを足した先へ送る。
    ///
    /// **書かなければ送らない** (機能ごと無効)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// 合言葉を置いたファイル。
    ///
    /// 設定に直接書かせないのは、設定ファイルが人の目に触れる場所 (共有した
    /// 土台・差分・貼り付けたログ) を通りやすいため。
    #[serde(default = "default_token_file")]
    pub token_file: PathBuf,
}

impl Default for Webhook {
    fn default() -> Self {
        Self {
            base_url: None,
            token_file: default_token_file(),
        }
    }
}

impl Webhook {
    /// 設定された根に、受け取る側が決めたパスを足す。
    pub fn destination_url(&self) -> std::result::Result<Option<url::Url>, String> {
        let Some(base) = &self.base_url else {
            return Ok(None);
        };
        let mut url =
            url::Url::parse(base).map_err(|e| format!("webhook.base_url is not a URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("webhook.base_url must be http or https".to_owned());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("webhook.base_url must not contain userinfo".to_owned());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("webhook.base_url must not contain a query or fragment".to_owned());
        }
        url.path_segments_mut()
            .map_err(|_| "webhook.base_url cannot be used as a base URL".to_owned())?
            .pop_if_empty()
            .push("webhook")
            .push("llm-gateway");
        Ok(Some(url))
    }
}

/// 既定の合言葉の置き場。受け取る側 (ccmsg) が置く場所に合わせる。
fn default_token_file() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
        .join("ccmsg")
        .join("webhook-llm-gateway.token")
}

/// 認証情報の置き場。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Store {
    /// 1 認証情報 1 ファイル。平文。
    File {
        /// 省略時は `$XDG_STATE_HOME/llm-gateway/credentials`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<PathBuf>,
    },
}

impl Default for Store {
    fn default() -> Self {
        Self::File { dir: None }
    }
}

impl Store {
    /// 実際に使う置き場。
    pub fn resolve_dir(&self) -> PathBuf {
        match self {
            Self::File { dir: Some(d) } => d.clone(),
            Self::File { dir: None } => default_credentials_dir(),
        }
    }
}

/// 使用量の日次集計の置き場 (DR-0011)。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stats {
    /// 省略時は `$XDG_STATE_HOME/llm-gateway/stats`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

impl Stats {
    /// 実際に使う置き場。
    pub fn resolve_dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(default_stats_dir)
    }
}

/// 認証情報 1 件の宣言。
///
/// 秘密そのものは store 側の `<key>.json` が持つ。ここで決めるのは payload の形と
/// login / refresh の手順だけで、接続先や話す API は [`RouteSpec`] が持つ。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialSpec {
    /// Anthropic のサブスク OAuth。
    ClaudeOauth,
    /// ChatGPT のサブスク OAuth。
    CodexOauth,
    /// Bedrock の API key。
    BedrockApiKey,
}

impl CredentialSpec {
    /// config.toml と保存 payload に共通する種別名。
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ClaudeOauth => "claude_oauth",
            Self::CodexOauth => "codex_oauth",
            Self::BedrockApiKey => "bedrock_api_key",
        }
    }

    /// gateway の HTTP ページだけで再認可を完結できる種別か。
    pub fn supports_web_login(&self) -> bool {
        matches!(self, Self::ClaudeOauth)
    }
}

/// upstream service status の設定。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatusConfig {
    #[serde(default = "default_status_refresh_interval", with = "human_duration")]
    pub refresh_interval: Duration,
    #[serde(default = "default_status_stale_after", with = "human_duration")]
    pub stale_after: Duration,
    #[serde(default = "default_status_observation_ttl", with = "human_duration")]
    pub observation_ttl: Duration,
    #[serde(
        default = "default_status_failure_refresh_cooldown",
        with = "human_duration"
    )]
    pub failure_refresh_cooldown: Duration,
    #[serde(default = "default_status_request_timeout", with = "human_duration")]
    pub request_timeout: Duration,
    #[serde(default)]
    pub sources: BTreeMap<String, StatusSourceSpec>,
}
impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            refresh_interval: default_status_refresh_interval(),
            stale_after: default_status_stale_after(),
            observation_ttl: default_status_observation_ttl(),
            failure_refresh_cooldown: default_status_failure_refresh_cooldown(),
            request_timeout: default_status_request_timeout(),
            sources: BTreeMap::new(),
        }
    }
}
fn default_status_refresh_interval() -> Duration {
    Duration::from_secs(60)
}
fn default_status_stale_after() -> Duration {
    Duration::from_secs(300)
}
fn default_status_observation_ttl() -> Duration {
    Duration::from_secs(300)
}
fn default_status_failure_refresh_cooldown() -> Duration {
    Duration::from_secs(30)
}
fn default_status_request_timeout() -> Duration {
    Duration::from_secs(5)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StatusSourceSpec {
    StatuspageV2 {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        summary_url: url::Url,
        incidents_url: url::Url,
        page_url: url::Url,
        #[serde(default)]
        components: Vec<String>,
    },
    Link {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        page_url: url::Url,
    },
}
impl StatusSourceSpec {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::StatuspageV2 { name, .. } | Self::Link { name, .. } => name.as_deref(),
        }
    }
    pub fn page_url(&self) -> &url::Url {
        match self {
            Self::StatuspageV2 { page_url, .. } | Self::Link { page_url, .. } => page_url,
        }
    }
    fn urls(&self) -> Vec<&url::Url> {
        match self {
            Self::StatuspageV2 {
                summary_url,
                incidents_url,
                page_url,
                ..
            } => vec![summary_url, incidents_url, page_url],
            Self::Link { page_url, .. } => vec![page_url],
        }
    }
}

mod human_duration {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &Duration,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&humantime::format_duration(*value).to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Duration, D::Error> {
        let text = String::deserialize(deserializer)?;
        humantime::parse_duration(&text).map_err(serde::de::Error::custom)
    }
}

mod optional_human_duration {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &Option<Duration>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match value {
            Some(value) => {
                serializer.serialize_some(&humantime::format_duration(*value).to_string())
            }
            None => serializer.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<Duration>, D::Error> {
        let text = Option::<String>::deserialize(deserializer)?;
        text.map(|text| humantime::parse_duration(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// upstream へ出る 1 経路。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// 話す API。Auth は `credential` が指す認証情報の形から独立して選ぶ。
    pub provider: Provider,
    /// store から使う認証情報。転送先が認証を持つ経路では省略する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// 接続先。省略時は provider の既定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// 公式状態を取得する status source。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_source: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// upstream が受け付けない beta フラグ。Anthropic 方言だけが使う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_beta: Option<Vec<String>>,
    /// discovery できない経路で公開するモデル。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// この経路で扱わないモデル。namespace の exclude に足して効く。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// upstream の API 方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Anthropic,
    Openai,
}

impl RouteSpec {
    pub fn url(&self) -> &str {
        self.url.as_deref().unwrap_or(match self.provider {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::Openai => "https://chatgpt.com/backend-api/codex",
        })
    }

    pub fn credential<'a>(&self, config: &'a Config) -> Option<&'a CredentialSpec> {
        self.credential
            .as_deref()
            .and_then(|name| config.credentials.get(name))
    }

    pub fn needs_secret(&self) -> bool {
        self.credential.is_some()
    }

    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    pub fn declared_models(&self) -> &[String] {
        &self.models
    }

    pub fn discovery_flavor(&self, config: &Config) -> Option<crate::discovery::Flavor> {
        match (self.provider, self.credential(config)) {
            (Provider::Anthropic, Some(CredentialSpec::ClaudeOauth)) => {
                Some(crate::discovery::Flavor::Anthropic)
            }
            (Provider::Anthropic, Some(CredentialSpec::BedrockApiKey)) => {
                Some(crate::discovery::Flavor::Bedrock)
            }
            (Provider::Openai, Some(CredentialSpec::CodexOauth)) => {
                Some(crate::discovery::Flavor::OpenAiCodex)
            }
            (Provider::Anthropic, None)
            | (Provider::Anthropic, Some(CredentialSpec::CodexOauth))
            | (Provider::Openai, _) => None,
        }
    }
}

fn validate_route(name: &str, route: &RouteSpec, config: &Config) -> Result<()> {
    let credential = route.credential(config);
    let supported = matches!(
        (route.provider, credential),
        (Provider::Anthropic, Some(CredentialSpec::ClaudeOauth))
            | (Provider::Anthropic, Some(CredentialSpec::BedrockApiKey))
            | (Provider::Anthropic, None)
            | (Provider::Openai, Some(CredentialSpec::CodexOauth))
    );
    if supported {
        return Ok(());
    }
    let credential_type = credential.map_or("none", CredentialSpec::type_name);
    Err(Error::Config(format!(
        "route `{name}` cannot combine provider `{:?}` with credential type `{credential_type}`",
        route.provider
    )))
}

impl Config {
    /// 読み込む。設定に矛盾があればここで弾く。
    pub fn load(path: &Path) -> Result<Self> {
        // 土台 (`extends`) があれば先に畳む (DR-0013)。
        let merged = extends::resolve(path)?;
        let config: Self = merged.try_into().map_err(|e| {
            Error::Config(format!(
                "{} is invalid: {e} (seen after merging with its base)",
                path.display()
            ))
        })?;
        config.validate()?;
        Ok(config)
    }

    /// 参照先が揃っているかを確かめる。
    ///
    /// 起動時に落としておかないと、その規則を最初に踏んだ人が
    /// 500 を見るまで誰も気づかない。
    pub fn validate(&self) -> Result<()> {
        self.webhook.destination_url().map_err(Error::Config)?;
        for (name, source) in &self.status.sources {
            if name.is_empty() {
                return Err(Error::Config("status source name is empty".to_owned()));
            }
            for url in source.urls() {
                if url.scheme() != "https" {
                    return Err(Error::Config(format!(
                        "status source `{name}` URL must use HTTPS: {url}"
                    )));
                }
            }
        }
        for (name, route) in &self.routes {
            if name.is_empty() {
                return Err(Error::Config("route name is empty".to_owned()));
            }
            if let Some(credential) = &route.credential
                && !self.credentials.contains_key(credential)
            {
                return Err(Error::Config(format!(
                    "route `{name}` references credential `{credential}`, which is not defined"
                )));
            }
            if let Some(source) = &route.status_source
                && !self.status.sources.contains_key(source)
            {
                return Err(Error::Config(format!(
                    "route `{name}` references status source `{source}`, which is not defined"
                )));
            }
            validate_route(name, route, self)?;
        }
        for (name, ns) in &self.namespaces {
            if name.is_empty() {
                return Err(Error::Config("namespace name is empty".to_owned()));
            }
            self.validate_namespace(name, ns)?;
        }
        Ok(())
    }

    fn validate_namespace(&self, ns_name: &str, ns: &Namespace) -> Result<()> {
        let known = |name: &String| self.routes.contains_key(name);

        for name in &ns.routes {
            if !known(name) {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` references route `{name}`, which is not defined"
                )));
            }
        }
        for (i, rule) in ns.cache.iter().enumerate() {
            if rule.models.is_empty() {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` cache[{i}] does not specify models"
                )));
            }
            if rule.sub == CacheStrategy::Keepalive {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` cache[{i}] cannot use keepalive for sub requests"
                )));
            }
        }
        for (i, rule) in ns.routing.iter().enumerate() {
            if rule.models.is_empty() {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` routing[{i}] does not specify models"
                )));
            }
            if rule.routes.is_empty() {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` routing[{i}] ({}) does not specify routes",
                    rule.models.join(", ")
                )));
            }
            if rule
                .routes
                .iter()
                .any(|group| matches!(group, RouteGroup::Equal(routes) if routes.is_empty()))
            {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` routing[{i}] ({}) contains an empty route group",
                    rule.models.join(", ")
                )));
            }
            for name in rule.routes.iter().flat_map(RouteGroup::routes) {
                if !self.routes.contains_key(name) {
                    return Err(Error::Config(format!(
                        "namespace `{ns_name}` routing[{i}] ({}) references route `{name}`, which is not defined",
                        rule.models.join(", ")
                    )));
                }
            }
            // 上限は同格グループの中にも書けるので、全要素を見る。
            for entry in rule.routes.iter().flat_map(RouteGroup::entries) {
                let Some(step) = entry.pace_cap().and_then(|cap| cap.step) else {
                    continue;
                };
                // 窓長は実行時にしか分からないので、割合指定は 0 でないこと
                // しか見られない。絶対指定は書かれた値をそのまま見る。
                let never_grows = match step {
                    WindowSpan::Absolute(seconds) => seconds == 0,
                    WindowSpan::Ratio(share) => share.as_fraction() == 0.0,
                };
                if never_grows {
                    return Err(Error::Config(format!(
                        "namespace `{ns_name}` routing[{i}] route `{}` has a pace_cap step of zero; the budget would never grow",
                        entry.name()
                    )));
                }
            }
        }
        Ok(())
    }

    /// 名前で namespace を引く。書いていなければ無い。
    ///
    /// パスに namespace が無いリクエストは `default` を引く。`[ns.default]` を
    /// 書かなければここで None になり、呼び出し側が 404 を返す (DR-0006)。
    pub fn namespace(&self, name: &str) -> Option<&Namespace> {
        self.namespaces.get(name)
    }

    /// 公開している namespace 名。
    pub fn namespace_names(&self) -> Vec<&str> {
        self.namespaces.keys().map(String::as_str).collect()
    }

    /// 既定の設定ファイルの場所。
    pub fn default_path() -> PathBuf {
        xdg_dir("XDG_CONFIG_HOME", ".config")
            .join("llm-gateway")
            .join("config.toml")
    }
}

/// パスに namespace が無いときに使う名前。
pub const DEFAULT_NAMESPACE: &str = "default";

impl Namespace {
    /// 実際に使うエイリアス。
    pub fn resolved_aliases(&self) -> BTreeMap<String, String> {
        let mut all = crate::discovery::default_aliases();
        all.extend(self.aliases.clone());
        all
    }

    /// このモデルを扱う経路を、試す順に返す。
    ///
    /// `routing` の上から照合し、最初に当たった規則を使う。当たらなければ
    /// この namespace が使える経路を宣言順に全部試す。
    pub fn routes_for<'a>(&'a self, model: &str, all: &'a Config) -> Vec<&'a str> {
        self.route_groups_for(model, all)
            .into_iter()
            .flatten()
            .collect()
    }

    /// このモデルを扱う経路を、同格グループの境界を保って返す。
    pub(crate) fn route_groups_for<'a>(
        &'a self,
        model: &str,
        all: &'a Config,
    ) -> Vec<Vec<&'a str>> {
        match self.rule_for(model) {
            Some(rule) => rule
                .routes
                .iter()
                .map(|group| group.routes().collect())
                .collect(),
            None => self
                .usable_routes(all)
                .into_iter()
                .map(|route| vec![route])
                .collect(),
        }
    }

    /// このモデルに当たる振り分け規則。当たらなければ無い。
    fn rule_for(&self, model: &str) -> Option<&RoutingRule> {
        self.routing
            .iter()
            .find(|rule| crate::pattern::matches_any(&rule.models, model))
    }

    /// このモデルに適用する prompt cache 規則。
    ///
    /// `cache` の上から照合し、最初に当たった規則を使う。
    pub fn cache_for(&self, model: &str) -> Option<&CacheRule> {
        self.cache
            .iter()
            .find(|rule| crate::pattern::matches_any(&rule.models, model))
    }

    /// このモデルで使い切りの繰り上げに入る手前の幅 (DR-0018)。
    ///
    /// 規則に当たらないモデル (= 宣言順に全部試す退化形) では繰り上げない。
    /// 順序を書いていない相手の順序を動かす理由がない。
    pub fn spend_down_for(&self, model: &str) -> Option<WindowSpan> {
        self.rule_for(model)?.spend_down_within
    }

    /// このモデルをこの経路へ流すときの消費上限 (DR-0019)。
    ///
    /// 書いていない経路には上限がない。同じ経路でも、規則ごとに (= モデル群
    /// ごとに、namespace ごとに) 違う上限を書ける。
    pub fn pace_cap_for(&self, model: &str, route: &str) -> Option<PaceCap> {
        self.rule_for(model)?
            .routes
            .iter()
            .flat_map(RouteGroup::entries)
            .find(|entry| entry.name() == route)?
            .pace_cap()
    }

    /// この namespace が使ってよい経路。
    pub fn usable_routes<'a>(&'a self, all: &'a Config) -> Vec<&'a str> {
        if self.routes.is_empty() {
            return all.routes.keys().map(String::as_str).collect();
        }
        self.routes.iter().map(String::as_str).collect()
    }

    /// この経路がこのモデルを扱ってよいか。
    pub fn allows(&self, route: &str, model: &str, all: &Config) -> bool {
        if crate::pattern::matches_any(&self.filter.exclude, model) {
            return false;
        }
        if !self.usable_routes(all).contains(&route) {
            return false;
        }
        match all.routes.get(route) {
            Some(spec) => !crate::pattern::matches_any(spec.exclude(), model),
            None => false,
        }
    }

    /// クライアントが名乗ったトークンを検査する。
    ///
    /// `auth_token` を書いていない namespace は、トークンの有無にかかわらず
    /// 通す (DR-0006)。境界は手前で引く前提。
    pub fn authorize(&self, presented: Option<&str>) -> Authorization {
        let Some(expected) = &self.auth_token else {
            return Authorization::Open;
        };
        // `Bearer xxx` でも `xxx` でも受ける。クライアントによって送り方が違う。
        let matched = presented.is_some_and(|p| {
            let p = p.strip_prefix("Bearer ").unwrap_or(p).trim();
            p == expected
        });
        if matched {
            Authorization::Accepted
        } else {
            Authorization::WrongToken
        }
    }
}

/// トークン検査の結果。
///
/// bool で返すと「なぜ通ったか」が消える。トークンが合って通ったのと、
/// そもそも検査していないのとでは意味が違い、記録に残す価値も違う
/// (DR-0006)。列挙にしておくと `match` が網羅を強制するので、通す枝と
/// 拒む枝のどちらも書き落とせない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorization {
    /// トークンが合っている。
    Accepted,
    /// トークンが違う (名乗っていない場合を含む)。
    WrongToken,
    /// この namespace は誰でも通す (`auth_token` を書いていない)。
    Open,
}

/// 既定の認証情報の置き場。
///
/// cache でなく state に置く。refresh token は消えると再ログインが要るので、
/// 「消えても再生成できる」cache の扱いに合わない。
pub fn default_credentials_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
        .join("llm-gateway")
        .join("credentials")
}

/// 既定の日次集計の置き場。
///
/// 認証情報と同じく state に置く。過去日の集計は消えると**作り直せない**
/// (upstream に聞き直す口が無い) ので、cache の扱いに合わない (DR-0011)。
pub fn default_stats_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
        .join("llm-gateway")
        .join("stats")
}

fn xdg_dir(env: &str, fallback: &str) -> PathBuf {
    std::env::var_os(env)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 土台を重ねた設定が、そのまま読み込めるところまで通る。
    ///
    /// 待ち受け先だけが違う 2 台を、同じ中身を書き写さずに用意できる
    /// (DR-0013)。畳んだ後の姿で項目名の検査まで通ることを、ここで見る。
    #[test]
    fn a_config_can_stand_on_another_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("shared.toml"),
            r#"
[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:8320"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]

[ns.default]
auth_token = "t"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("unstable.toml"),
            "extends = \"shared.toml\"\n\n[server]\nlisten = \"127.0.0.1:8402\"\n",
        )
        .unwrap();

        let config = Config::load(&dir.path().join("unstable.toml")).unwrap();
        assert_eq!(
            config.server.listen, "127.0.0.1:8402",
            "what it wrote itself"
        );
        assert_eq!(
            config.namespace("default").unwrap().auth_token.as_deref(),
            Some("t"),
            "what comes from the base"
        );
        assert!(config.routes.contains_key("a"));
    }

    /// 実運用を想定した一式。
    const SAMPLE: &str = r#"
[server]
listen = "127.0.0.1:8317"

[store]
type = "file"

[credentials.claude-personal]
type = "claude_oauth"

[credentials.claude-work-a]
type = "claude_oauth"

[credentials.claude-work-b]
type = "claude_oauth"

[credentials.bedrock]
type = "bedrock_api_key"

[routes.claude-personal]
provider = "anthropic"
credential = "claude-personal"

[routes.claude-work-a]
provider = "anthropic"
credential = "claude-work-a"

# fable 専用のアカウント。
[routes.claude-work-b]
provider = "anthropic"
credential = "claude-work-b"
exclude = ["claude-opus-*", "claude-sonnet-*", "claude-haiku-*"]

[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"

[routes.cpa]
provider = "anthropic"
url = "http://127.0.0.1:8320"
models = ["gpt-*"]

# 古い世代は出さない。
[ns.default.filter]
exclude = ["claude-3-*", "claude-opus-4*", "claude-sonnet-4-*"]

# fable は Bedrock 優先 (Claude アカウントを消費しない)。
[[ns.default.routing]]
models = ["claude-fable-*"]
routes = ["bedrock", "claude-work-b", "claude-personal"]

[[ns.default.routing]]
models = ["gpt-*"]
routes = ["cpa"]

[ns.default.aliases]
o = "claude-opus-*"
"#;

    fn parse(s: &str) -> Result<Config> {
        let c: Config = toml::from_str(s).map_err(|e| Error::Config(e.to_string()))?;
        c.validate()?;
        Ok(c)
    }

    /// 既定の namespace。`/v1/...` が解決される先。
    pub(super) fn ns(c: &Config) -> &Namespace {
        c.namespace(DEFAULT_NAMESPACE)
            .expect("this config has [ns.default]")
    }

    /// `thinking_display` は API が受け付ける 2 値だけを設定できる。
    ///
    /// 文字列のまま保持せず列挙型に閉じることで、不正値を起動前の設定解析で弾く。
    #[test]
    fn thinking_display_accepts_only_supported_values() {
        for (raw, expected) in [
            ("summarized", ThinkingDisplay::Summarized),
            ("omitted", ThinkingDisplay::Omitted),
        ] {
            let config = parse(&format!("[ns.default]\nthinking_display = \"{raw}\"\n"))
                .unwrap_or_else(|e| panic!("{raw} should be valid: {e}"));
            assert_eq!(ns(&config).thinking_display, Some(expected), "{raw}");
        }

        let error = parse("[ns.default]\nthinking_display = \"visible\"\n")
            .expect_err("an out-of-enum value is a config error");
        let message = error.to_string();
        assert!(message.contains("unknown variant `visible`"), "{message}");
        assert!(
            message.contains("summarized"),
            "lists the accepted values: {message}"
        );
        assert!(
            message.contains("omitted"),
            "lists the accepted values: {message}"
        );
    }

    /// `auth_token` を書かない namespace は、名乗り方に関わらず誰でも通す。
    ///
    /// 境界は手前 (tailnet / リバースプロキシ) で引く前提 (DR-0006)。
    #[test]
    fn namespace_without_token_accepts_everyone() {
        let ns = Namespace::default();
        assert_eq!(ns.auth_token, None, "the unwritten state");

        for presented in [None, Some("anything"), Some("Bearer anything"), Some("")] {
            assert_eq!(
                ns.authorize(presented),
                Authorization::Open,
                "{presented:?} passes too"
            );
        }
    }

    /// 書いてあれば、合っているものだけ通す。
    ///
    /// `Bearer xxx` と裸の `xxx` の両方を受けるのは、クライアントによって
    /// 送り方が違うため。
    #[test]
    fn configured_token_is_matched_both_ways() {
        let ns = Namespace {
            auth_token: Some("s3cret".to_owned()),
            ..Namespace::default()
        };

        for ok in ["s3cret", "Bearer s3cret"] {
            assert_eq!(ns.authorize(Some(ok)), Authorization::Accepted, "{ok}");
        }
        for bad in ["", "nope", "Bearer nope", "bearer s3cret", "s3cretx"] {
            assert_eq!(ns.authorize(Some(bad)), Authorization::WrongToken, "{bad}");
        }
        assert_eq!(
            ns.authorize(None),
            Authorization::WrongToken,
            "not identifying is treated the same as a mismatch"
        );
    }

    /// 書いていない `default` は存在しない。`/v1/...` は 404 になる。
    #[test]
    fn default_namespace_is_not_conjured_up() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[ns.personal]
"#,
        )
        .unwrap();

        assert!(c.namespace(DEFAULT_NAMESPACE).is_none());
        assert_eq!(
            c.namespace_names(),
            vec!["personal"],
            "only what was written is exposed"
        );
    }

    #[test]
    fn reads_a_full_config() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:8317");
        assert_eq!(c.credentials.len(), 4);
        assert_eq!(c.routes.len(), 5);
        assert_eq!(ns(&c).routing.len(), 2);
        assert_eq!(c.discovery.refresh_secs, 3600, "default is 1 hour");
    }

    /// モデル一覧を手で書かない。upstream に聞くので、設定にあるのは
    /// 「隠すもの」と「特別扱いするもの」だけ。
    #[test]
    fn no_model_list_is_required() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[ns.default]
"#,
        )
        .unwrap();
        assert!(ns(&c).routing.is_empty());
        assert!(ns(&c).filter.exclude.is_empty());
    }

    /// 全体の exclude は全 credential に効く。
    #[test]
    fn global_exclude_hides_old_generations() {
        let c = parse(SAMPLE).unwrap();
        for hidden in [
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            "claude-3-5-sonnet-20241022",
        ] {
            assert!(
                !ns(&c).allows("claude-personal", hidden, &c),
                "{hidden} is hidden"
            );
        }
        for shown in ["claude-opus-5", "claude-sonnet-5", "claude-fable-5"] {
            assert!(
                ns(&c).allows("claude-personal", shown, &c),
                "{shown} is shown"
            );
        }
    }

    /// credential ごとの exclude はその credential にだけ効く。
    #[test]
    fn per_credential_exclude_is_scoped() {
        let c = parse(SAMPLE).unwrap();
        assert!(
            !ns(&c).allows("claude-work-b", "claude-opus-5", &c),
            "fable-only, so opus is not handled"
        );
        assert!(
            ns(&c).allows("claude-work-b", "claude-fable-5", &c),
            "fable is handled"
        );
        assert!(
            ns(&c).allows("claude-personal", "claude-opus-5", &c),
            "has no effect on other credentials"
        );
    }

    #[test]
    fn unknown_credential_is_not_allowed() {
        let c = parse(SAMPLE).unwrap();
        assert!(!ns(&c).allows("no-such-credential", "claude-opus-5", &c));
    }

    /// ルーティングはパターンで書ける。最初に当たった規則を使う。
    #[test]
    fn routing_matches_by_pattern() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            ns(&c).routes_for("claude-fable-5", &c),
            vec!["bedrock", "claude-work-b", "claude-personal"]
        );
        assert_eq!(ns(&c).routes_for("gpt-5.6-sol", &c), vec!["cpa"]);
    }

    /// フラット配列は全 route が単独グループである従来記法として、そのまま読める。
    #[test]
    fn routing_accepts_flat_route_groups() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            ns(&c).route_groups_for("claude-fable-5", &c),
            vec![
                vec!["bedrock"],
                vec!["claude-work-b"],
                vec!["claude-personal"]
            ]
        );
    }

    /// 内側の配列は同格グループで、外側の文字列は単独グループとして境界を保つ。
    #[test]
    fn routing_accepts_equal_route_groups() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"
[credentials.b]
type = "claude_oauth"
[credentials.c]
type = "claude_oauth"
[routes.a]
provider = "anthropic"
credential = "a"
[routes.b]
provider = "anthropic"
credential = "b"
[routes.c]
provider = "anthropic"
credential = "c"
[[ns.default.routing]]
models = ["model"]
routes = [["a", "b"], "c"]
"#,
        )
        .unwrap();
        assert_eq!(
            ns(&c).route_groups_for("model", &c),
            vec![vec!["a", "b"], vec!["c"]]
        );
    }

    // ---------- 使い切りの閾値 (DR-0018) ----------

    /// 割合は窓長に掛ける係数として読む。`25%` は 7d 枠なら 42 時間。
    #[test]
    fn spend_down_reads_a_percentage_against_the_window() {
        let within: WindowSpan = "25%".parse().unwrap();
        assert_eq!(within, WindowSpan::Ratio("25%".parse().unwrap()));

        const WEEK: u64 = 7 * 24 * 60 * 60;
        assert_eq!(within.seconds_in(WEEK), 42 * 60 * 60);
        // 同じ設定でも、窓が短ければ閾値も短くなる。
        assert_eq!(within.seconds_in(5 * 60 * 60), 75 * 60);
    }

    /// 絶対時間は窓長に関係なく同じ幅になる。
    #[test]
    fn spend_down_reads_an_absolute_duration() {
        let within: WindowSpan = "40h".parse().unwrap();
        assert_eq!(within, WindowSpan::Absolute(40 * 60 * 60));
        assert_eq!(within.seconds_in(7 * 24 * 60 * 60), 40 * 60 * 60);
        assert_eq!(within.seconds_in(5 * 60 * 60), 40 * 60 * 60);

        // humantime の複合表記も読める。
        assert_eq!(
            "2d 6h".parse::<WindowSpan>().unwrap(),
            WindowSpan::Absolute((2 * 24 + 6) * 60 * 60)
        );
    }

    /// 割合でも時間でもない書き方は、設定を読んだ時点で拒否する。
    #[test]
    fn spend_down_rejects_anything_else() {
        for text in ["25x", "%", "", "twenty-five percent", "-1%", "101%", "40"] {
            assert!(text.parse::<WindowSpan>().is_err(), "{text}");
        }
    }

    /// 規則に書けば読め、書かなければ繰り上げない。
    #[test]
    fn spend_down_is_read_per_routing_rule() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"
[routes.a]
provider = "anthropic"
credential = "a"
[[ns.default.routing]]
models = ["spent-*"]
routes = ["a"]
spend_down_within = "25%"
[[ns.default.routing]]
models = ["plain-*"]
routes = ["a"]
"#,
        )
        .unwrap();
        assert_eq!(
            ns(&c).spend_down_for("spent-1"),
            Some(WindowSpan::Ratio("25%".parse().unwrap()))
        );
        assert_eq!(ns(&c).spend_down_for("plain-1"), None);
        // どの規則にも当たらないモデルは、順序を書いていないので動かさない。
        assert_eq!(ns(&c).spend_down_for("other"), None);
    }

    // ---------- 消費の上限 (DR-0019) ----------

    const WEEK: u64 = 7 * 24 * 60 * 60;

    fn capped() -> Config {
        parse(
            r#"
[credentials.a]
type = "claude_oauth"
[credentials.b]
type = "claude_oauth"
[credentials.c]
type = "claude_oauth"
[routes.a]
provider = "anthropic"
credential = "a"
[routes.b]
provider = "anthropic"
credential = "b"
[routes.c]
provider = "anthropic"
credential = "c"
[[ns.default.routing]]
models = ["model"]
routes = [
    { route = "a", pace_cap = { ratio = "100%", step = "12h" } },
    ["b", { route = "c", pace_cap = { ratio = "80%" } }],
]
"#,
        )
        .unwrap()
    }

    /// table で書いた要素も、文字列で書いた要素と同じ位置を占める。
    ///
    /// 同格グループの中に混ぜても、グループの境界は変わらない。
    #[test]
    fn routing_accepts_attributed_routes_anywhere_a_name_goes() {
        let c = capped();
        assert_eq!(
            ns(&c).route_groups_for("model", &c),
            vec![vec!["a"], vec!["b", "c"]]
        );
        assert_eq!(ns(&c).routes_for("model", &c), vec!["a", "b", "c"]);
    }

    /// 上限は経路ごと。同格グループの中に書いた分も読める (DR-0019 §1)。
    #[test]
    fn pace_cap_is_read_per_route_including_inside_groups() {
        let c = capped();
        let outer = ns(&c).pace_cap_for("model", "a").expect("written");
        assert_eq!(outer.ratio, Some("100%".parse().unwrap()));
        assert_eq!(outer.step, Some(WindowSpan::Absolute(12 * 60 * 60)));

        let inner = ns(&c)
            .pace_cap_for("model", "c")
            .expect("written in a group");
        assert_eq!(inner.ratio, Some("80%".parse().unwrap()));
        assert_eq!(inner.step, None, "the default step applies");

        assert_eq!(ns(&c).pace_cap_for("model", "b"), None);
        // 規則に当たらないモデルには上限がない。
        assert_eq!(ns(&c).pace_cap_for("other", "a"), None);
    }

    /// 解放は書いたときだけ立つ。既定は保護 (解放しない)。
    #[test]
    fn releasing_at_spend_down_is_opt_in() {
        let c = capped();
        assert!(
            !ns(&c)
                .pace_cap_for("model", "a")
                .unwrap()
                .spend_down_release,
            "not written, so the cap holds to the end"
        );

        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"
[routes.a]
provider = "anthropic"
credential = "a"
[[ns.default.routing]]
models = ["model"]
routes = [{ route = "a", pace_cap = { spend_down_release = true } }]
spend_down_within = "25%"
"#,
        )
        .unwrap();
        let cap = ns(&c).pace_cap_for("model", "a").unwrap();
        assert!(cap.spend_down_release);
        // 他の欄は既定のまま書かずに済む。
        assert_eq!(cap.ratio, None);
        assert_eq!(cap.step, None);
    }

    /// 何も書かない上限は「按分線ちょうど / 窓長の 1/14 刻み」。
    #[test]
    fn an_empty_pace_cap_uses_the_defaults() {
        let cap = PaceCap::default();
        // 7d 窓なら 12 時間ごと。
        assert_eq!(cap.budget(WEEK, 0).next_step_at, 12 * 60 * 60);
        // 半分まで経てば半分。ratio 100% なので按分線ちょうど。
        assert_eq!(cap.budget(WEEK, WEEK as i64 / 2).allowed, 0.5);
    }

    /// 予算は段の上でだけ増える。同じ段にいる間は動かない。
    #[test]
    fn the_budget_grows_only_at_step_boundaries() {
        let cap = PaceCap {
            ratio: None,
            spend_down_release: false,
            step: Some(WindowSpan::Absolute(24 * 60 * 60)),
        };
        const DAY: i64 = 24 * 60 * 60;

        // 1 日目の途中はまだ 0 段目。使ってよい量はゼロ。
        let start = cap.budget(WEEK, DAY - 1);
        assert_eq!(start.allowed, 0.0);
        assert_eq!(start.next_step_at, DAY, "the budget grows one day in");

        // 2 日目に入ると 1 日ぶん = 1/7。
        let second = cap.budget(WEEK, DAY);
        assert!((second.allowed - 1.0 / 7.0).abs() < 1e-9);
        assert_eq!(second.next_step_at, 2 * DAY);

        // 2 日目のどこにいても同じ予算。
        assert_eq!(cap.budget(WEEK, 2 * DAY - 1), second);
    }

    /// ratio は予算を丸ごと絞る。按分線より手前で締める方向にだけ効く。
    #[test]
    fn the_ratio_tightens_the_whole_budget() {
        const DAY: i64 = 24 * 60 * 60;
        let cap = |ratio: &str| PaceCap {
            ratio: Some(ratio.parse().unwrap()),
            spend_down_release: false,
            step: Some(WindowSpan::Absolute(DAY as u64)),
        };
        let on_pace = cap("100%").budget(WEEK, 3 * DAY).allowed;
        let tighter = cap("80%").budget(WEEK, 3 * DAY).allowed;

        assert!((on_pace - 3.0 / 7.0).abs() < 1e-9, "the elapsed share");
        assert!((tighter - on_pace * 0.8).abs() < 1e-9);
        // 段の位置は ratio に影響されない。
        assert_eq!(
            cap("100%").budget(WEEK, 3 * DAY).next_step_at,
            cap("80%").budget(WEEK, 3 * DAY).next_step_at
        );
    }

    /// 按分線を超える貸し出しは書けない。仕事枠の保護が目的なので。
    #[test]
    fn a_ratio_above_the_pace_line_is_rejected() {
        for text in ["120%", "101%", "-1%", "1.0", "abc", ""] {
            assert!(text.parse::<Share>().is_err(), "{text}");
        }
        assert_eq!("0%".parse::<Share>().unwrap().as_fraction(), 0.0);
        assert_eq!("100%".parse::<Share>().unwrap().as_fraction(), 1.0);
    }

    /// step を割合で書くと、窓長に合わせて刻みが変わる。
    #[test]
    fn a_percentage_step_scales_to_the_window() {
        let cap = PaceCap {
            ratio: None,
            spend_down_release: false,
            step: Some(WindowSpan::Ratio("25%".parse().unwrap())),
        };
        // 7d 窓の 25 % = 42 時間ごと。
        assert_eq!(cap.budget(WEEK, 0).next_step_at, 42 * 60 * 60);
        assert_eq!(cap.budget(WEEK, 42 * 60 * 60).allowed, 0.25);
    }

    /// 窓を出た経過は窓の端で止める。予算が ratio を超えて伸びない。
    #[test]
    fn the_budget_stops_at_the_end_of_the_window() {
        let cap = PaceCap {
            ratio: None,
            spend_down_release: false,
            step: Some(WindowSpan::Ratio("50%".parse().unwrap())),
        };
        assert_eq!(cap.budget(WEEK, WEEK as i64 * 2).allowed, 1.0);
        assert_eq!(cap.budget(WEEK, -100).allowed, 0.0);
    }

    /// 予算が増えない書き方 (刻み 0) は、設定を読んだ時点で拒否する。
    #[test]
    fn a_pace_cap_that_never_grows_is_rejected() {
        for step in ["\"0%\"", "\"0s\""] {
            let err = parse(&format!(
                r#"
[credentials.a]
type = "claude_oauth"
[routes.a]
provider = "anthropic"
credential = "a"
[[ns.default.routing]]
models = ["model"]
routes = [{{ route = "a", pace_cap = {{ step = {step} }} }}]
"#
            ))
            .unwrap_err();
            assert!(err.to_string().contains("never grow"), "{step}: {err}");
        }
    }

    /// 割合として書けない ratio は、設定を読んだ時点で拒否する。
    #[test]
    fn a_ratio_outside_the_range_is_rejected_in_the_config() {
        let err = parse(
            r#"
[credentials.a]
type = "claude_oauth"
[routes.a]
provider = "anthropic"
credential = "a"
[[ns.default.routing]]
models = ["model"]
routes = [{ route = "a", pace_cap = { ratio = "120%" } }]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("0%-100%"), "{err}");
    }

    /// グループの中にグループは書けない。ネストは同格を表す 1 段だけに限定する。
    #[test]
    fn routing_rejects_groups_nested_two_levels() {
        let err = parse(
            r#"
[[ns.default.routing]]
models = ["model"]
routes = [[["a"]]]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("routes"), "{err}");
    }

    /// 空の同格グループは候補を持たず設定ミスを隠すため、起動時に拒否する。
    #[test]
    fn routing_rejects_empty_route_groups() {
        let err = parse(
            r#"
[[ns.default.routing]]
models = ["model"]
routes = [[]]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty route group"), "{err}");
    }

    /// 規則に当たらないモデルは宣言順に試す。
    /// 新しいモデルが出ても、設定を触らずに使える。
    #[test]
    fn unmatched_models_fall_back_to_declaration_order() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            ns(&c).routes_for("claude-opus-6", &c),
            vec![
                "bedrock",
                "claude-personal",
                "claude-work-a",
                "claude-work-b",
                "cpa"
            ],
            "declaration order (name order, since it's a BTreeMap)"
        );
    }

    /// 上に書いた規則が勝つ。
    #[test]
    fn first_matching_rule_wins() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[credentials.b]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"

[routes.b]
provider = "anthropic"
credential = "b"

[[ns.default.routing]]
models = ["claude-opus-5"]
routes = ["a"]

[[ns.default.routing]]
models = ["claude-*"]
routes = ["b"]
"#,
        )
        .unwrap();
        assert_eq!(
            ns(&c).routes_for("claude-opus-5", &c),
            vec!["a"],
            "a specific entry comes first"
        );
        assert_eq!(
            ns(&c).routes_for("claude-sonnet-5", &c),
            vec!["b"],
            "a broader rule comes after"
        );
    }

    /// エイリアスは設定に書いたものだけ。コードに既定は無い。
    #[test]
    fn aliases_come_only_from_config() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(ns(&c).resolved_aliases()["o"], "claude-opus-*");

        let empty = parse("[ns.default]").unwrap();
        assert!(
            ns(&empty).resolved_aliases().is_empty(),
            "none at all when unwritten"
        );
    }

    #[test]
    fn aliases_are_read_as_written() {
        let c = parse(
            r#"
[ns.default.aliases]
opus = "claude-opus-4*"
"#,
        )
        .unwrap();
        assert_eq!(ns(&c).resolved_aliases()["opus"], "claude-opus-4*");
    }

    /// 一覧を聞ける upstream と聞けない upstream。
    #[test]
    fn discovery_flavor_by_type() {
        use crate::discovery::Flavor;
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            c.routes["claude-personal"].discovery_flavor(&c),
            Some(Flavor::Anthropic)
        );
        assert_eq!(
            c.routes["bedrock"].discovery_flavor(&c),
            Some(Flavor::Bedrock)
        );
        assert_eq!(
            c.routes["cpa"].discovery_flavor(&c),
            None,
            "cannot be queried on a relay target"
        );
    }

    /// 聞けない upstream は設定に書いたモデルを使う。
    #[test]
    fn relay_declares_its_models() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.routes["cpa"].declared_models(), ["gpt-*"]);
        assert!(c.routes["claude-personal"].declared_models().is_empty());
    }

    #[test]
    fn unknown_credential_reference_is_rejected() {
        let err = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"

[[ns.default.routing]]
models = ["m"]
routes = ["a", "typo-here"]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("typo-here"), "{err}");
    }

    #[test]
    fn empty_routing_fields_are_rejected() {
        assert!(parse("[[ns.default.routing]]\nmodels = []\nroutes = []").is_err());
        assert!(
            parse("[[ns.default.routing]]\nmodels = [\"m\"]\nroutes = []")
                .unwrap_err()
                .to_string()
                .contains('m')
        );
    }

    /// 綴り間違いを黙って無視しない。
    #[test]
    fn unknown_key_is_rejected() {
        let err = parse("[server]\nlisen = \"typo\"").unwrap_err();
        assert!(err.to_string().contains("lisen"), "{err}");
    }

    #[test]
    fn unknown_credential_type_is_rejected() {
        let err = parse("[credentials.a]\ntype = \"no_such_type\"").unwrap_err();
        assert!(err.to_string().contains("no_such_type"), "{err}");
    }

    #[test]
    fn empty_config_is_valid() {
        let c = parse("").unwrap();
        assert_eq!(
            c.server.listen, "127.0.0.1:11300",
            "the default listen address"
        );
        assert!(!c.server.disabled, "listens when unwritten");
        assert!(matches!(c.store, Store::File { dir: None }));
    }

    /// 待ち受けないと書ける。住所 (`listen`) は書いても書かなくてもよい。
    ///
    /// `--config` を省いて CLI を使うための設定に使う (DR-0013)。
    #[test]
    fn a_config_can_declare_that_it_does_not_listen() {
        let c = parse("[server]\ndisabled = true\nlisten = \"127.0.0.1:8402\"").unwrap();
        assert!(c.server.disabled);
        assert_eq!(
            c.server.listen, "127.0.0.1:8402",
            "still remains as a queryable target"
        );
    }

    #[test]
    fn store_dir_can_be_overridden() {
        let c = parse("[store]\ntype = \"file\"\ndir = \"/tmp/creds\"").unwrap();
        assert_eq!(c.store.resolve_dir(), PathBuf::from("/tmp/creds"));
    }

    /// 既定は state 配下。cache ではない (消えると再ログインが要る)。
    #[test]
    fn default_store_dir_is_under_state() {
        let dir = Store::default().resolve_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with("llm-gateway/credentials"), "{s}");
        assert!(s.contains("state"), "{s}");
    }

    #[test]
    fn discovery_interval_can_be_set() {
        let c = parse("[discovery]\nrefresh_secs = 300").unwrap();
        assert_eq!(c.discovery.refresh_secs, 300);
    }

    /// 認証情報の更新を見に行く間隔も設定で変えられる。
    #[test]
    fn credential_watch_interval_can_be_set() {
        assert_eq!(
            parse("").unwrap().discovery.watch_secs,
            60,
            "default is one minute"
        );
        let c = parse("[discovery]\nwatch_secs = 5").unwrap();
        assert_eq!(c.discovery.watch_secs, 5);
    }

    #[test]
    fn webhook_is_disabled_by_default() {
        let c = parse("").unwrap();
        assert!(c.webhook.base_url.is_none());
        assert!(
            c.webhook
                .token_file
                .ends_with("ccmsg/webhook-llm-gateway.token")
        );
    }

    #[test]
    fn webhook_destination_and_token_file_can_be_set() {
        let c = parse(
            "[webhook]\nbase_url = \"http://127.0.0.1:1234/\"\ntoken_file = \"/tmp/hook.token\"",
        )
        .unwrap();
        assert_eq!(
            c.webhook.base_url.as_deref(),
            Some("http://127.0.0.1:1234/")
        );
        assert_eq!(c.webhook.token_file, PathBuf::from("/tmp/hook.token"));
        assert_eq!(
            c.webhook.destination_url().unwrap().unwrap().as_str(),
            "http://127.0.0.1:1234/webhook/llm-gateway"
        );
    }

    #[test]
    fn webhook_base_path_is_preserved() {
        let c = parse("[webhook]\nbase_url = \"https://example.test/hooks\"").unwrap();
        assert_eq!(
            c.webhook.destination_url().unwrap().unwrap().as_str(),
            "https://example.test/hooks/webhook/llm-gateway"
        );
    }

    #[test]
    fn unsafe_webhook_destinations_are_rejected() {
        for base in [
            "",
            "file:///tmp/hook",
            "https://user@example.test",
            "https://example.test?target=other",
            "https://example.test#other",
        ] {
            let source = format!("[webhook]\nbase_url = {base:?}");
            assert!(parse(&source).is_err(), "{base} was accepted");
        }
    }

    #[test]
    fn unknown_webhook_key_is_rejected() {
        let err = parse("[webhook]\nurl = \"http://127.0.0.1:1234\"").unwrap_err();
        assert!(err.to_string().contains("url"), "{err}");
    }

    #[test]
    fn round_trips_through_toml() {
        let original = parse(SAMPLE).unwrap();
        let again = parse(&toml::to_string(&original).unwrap()).unwrap();
        assert_eq!(ns(&again).routing.len(), ns(&original).routing.len());
        assert_eq!(ns(&again).filter.exclude, ns(&original).filter.exclude);
        assert_eq!(
            ns(&again).routes_for("claude-fable-5", &again),
            ns(&original).routes_for("claude-fable-5", &original)
        );
    }
}

#[cfg(test)]
mod example_tests {
    use super::tests::ns;
    use super::*;

    /// 配る雛形が実際に読めるか。
    ///
    /// 壊れていると `just init-config` の直後に起動できず、初めて使う人が
    /// 最初につまずく。設定の書き方を変えたときに追従を忘れやすい。
    #[test]
    fn shipped_example_is_valid() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../dist/config.example.toml"
        );
        let raw = std::fs::read_to_string(path).expect("dist/config.example.toml is required");
        let config: Config = toml::from_str(&raw).expect("the template is broken");
        config
            .validate()
            .expect("a reference in the template is broken");

        // 使い方を示す要素が一通り入っているか。
        assert!(
            !ns(&config).filter.exclude.is_empty(),
            "an example of hiding"
        );
        assert!(!ns(&config).routing.is_empty(), "an example of routing");
        assert!(
            !ns(&config).aliases.is_empty(),
            "an example of a short name"
        );
        assert!(
            ns(&config).auth_token.is_some(),
            "following the template as-is enables auth (unset means deny-all, so the example is needed)"
        );
        assert!(
            config
                .routes
                .values()
                .any(|route| !route.exclude().is_empty()),
            "an example of a per-credential restriction"
        );
        assert!(
            config
                .routes
                .values()
                .any(|route| !route.declared_models().is_empty()),
            "an example of declaring the models a relay handles"
        );
    }

    /// 雛形にモデル一覧を書かない。
    ///
    /// 一覧は upstream に聞くのが本筋で、雛形が手書きの例を示すと
    /// そちらに引きずられる。
    #[test]
    fn example_does_not_hardcode_a_model_list() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../dist/config.example.toml"
        );
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(
            !raw.contains("[models."),
            "a legacy-format model definition remains"
        );
    }
}
