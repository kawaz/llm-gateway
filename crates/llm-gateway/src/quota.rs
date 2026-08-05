//! credential ごとの利用状況 (DR-0007)。
//!
//! upstream は使用率を**応答ヘッダに載せて**返してくる。gateway は全ての
//! リクエストを仲介しているので、通りすがりに読んで credential ごとの最新
//! スナップショットを持てば、追加の API コールなしで一覧が作れる。
//!
//! 読むのはヘッダだけ。本文には触れないので、SSE の中継は今までどおり
//! バイト列のまま流れる。
//!
//! 弱点は「しばらく使っていない credential の値が古い / 無い」こと。だから
//! スナップショットには必ず取得時刻を付け、古さが見えるようにする。
//!
//! ## 再起動を跨いで持つ
//!
//! スナップショットはディスクにも落とす。落とさないと再起動のたびに全部が
//! 未観測へ戻り、次にその credential を使うまで何も見えない。古い値が残る
//! ことになるが、取得時刻が付いているので古さは読み手が判断できる (CLI は
//! 5 分を超えた分に経過を添える)。値そのものは「最後に観測したときの実測」で、
//! 推測ではない (kawaz 裁定 2026-07-31)。
//!
//! 書き込み先は日次集計と同じ置き場の、**このプロセス専用のファイル**
//! (`usage-latest.<書き手>.json`)。他の writer の分は読まない — 向こうの
//! 観測は向こうが持っている。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::credential::CredentialId;
use crate::credential::time::format_rfc3339;
use crate::limits::Limit;
use crate::persist::{sanitize_writer, sweep_temporaries, write_atomically};

/// 応答ヘッダから利用状況を読む道具。
///
/// upstream ごとにヘッダの名前も単位も違うので、読み手を並べておいて
/// 上から試す。新しい upstream に対応するときはここに 1 つ足す。
type Reader = fn(&[(String, String)], i64) -> Option<Snapshot>;

/// 対応している読み手。
///
/// 今 gateway が直接話す upstream は Anthropic 系だけ。Codex は副作用の無い
/// 専用の口 (`wham/usage`) があり、ヘッダを拾うより素直なので、そちらを
/// 実装するときに読み手ではなく別経路として足す (DR-0007)。
const READERS: &[Reader] = &[read_anthropic_unified];

/// 1 つの窓 (5 時間 / 7 日) の状況。
///
/// どれも欠けうる。unified ヘッダは公式ドキュメントに記載が無く、予告なく
/// 変わる前提で扱う (DR-0007) ので、読めないものは `None` にして残りを使う。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 使用率 (0.0〜1.0)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// `allowed` など。上限に達しているかが分かる。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// リセット時刻 (Unix 秒)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset: Option<i64>,
    /// 同じ時刻の ISO 8601 表記。人が読む側で変換し直さずに済む。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_iso: Option<String>,
}

impl Window {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn with_reset(mut self, reset: Option<i64>) -> Self {
        self.reset = reset;
        self.reset_iso = reset.map(format_rfc3339);
        self
    }
}

/// 従量課金へのフォールバックが使えるか。
///
/// 残クレジットの**数値**は取れない。使えるかどうかの 2 値だけ (DR-0007)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 塞がれている理由 (`out_of_credits` など)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

impl Overage {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// ある時点で観測した利用状況。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// 観測した時刻 (Unix 秒)。**必ず付ける** — 古さが見えないと、
    /// 使っていない credential の値を最新だと誤解する。
    pub observed_at: i64,
    pub observed_at_iso: String,
    #[serde(rename = "5h", skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<Window>,
    #[serde(rename = "7d", skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<Window>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage: Option<Overage>,
}

impl Snapshot {
    /// 応答ヘッダから読む。利用状況が 1 つも載っていなければ `None`。
    ///
    /// `None` を返すのは大事で、ここで空のスナップショットを作ると
    /// 「観測した (中身は空)」と「まだ観測していない」が区別できなくなる。
    pub fn from_headers(headers: &[(String, String)], now: i64) -> Option<Self> {
        READERS.iter().find_map(|read| read(headers, now))
    }
}

/// 名前の大小を無視して 1 つ引く。
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Anthropic の `anthropic-ratelimit-unified-*` を読む (実測値は DR-0007)。
fn read_anthropic_unified(headers: &[(String, String)], now: i64) -> Option<Snapshot> {
    let get = |name: &str| header(headers, name);
    let window = |prefix: &str| {
        let w = Window {
            utilization: get(&format!("anthropic-ratelimit-unified-{prefix}-utilization"))
                .and_then(|v| v.trim().parse().ok()),
            status: get(&format!("anthropic-ratelimit-unified-{prefix}-status")).map(str::to_owned),
            ..Window::default()
        }
        .with_reset(
            get(&format!("anthropic-ratelimit-unified-{prefix}-reset"))
                .and_then(|v| v.trim().parse().ok()),
        );
        (!w.is_empty()).then_some(w)
    };

    let overage = Overage {
        status: get("anthropic-ratelimit-unified-overage-status").map(str::to_owned),
        disabled_reason: get("anthropic-ratelimit-unified-overage-disabled-reason")
            .map(str::to_owned),
    };

    let snapshot = Snapshot {
        observed_at: now,
        observed_at_iso: format_rfc3339(now),
        five_hour: window("5h"),
        seven_day: window("7d"),
        overage: (!overage.is_empty()).then_some(overage),
    };

    // どれも読めなかった応答は、この upstream のものではない。
    if snapshot.five_hour.is_none() && snapshot.seven_day.is_none() && snapshot.overage.is_none() {
        return None;
    }
    Some(snapshot)
}

/// ディスクに落とす形。credential 名 → 最後に観測したスナップショット。
type Saved = BTreeMap<String, Snapshot>;

/// credential ごとの最新スナップショット。
///
/// メモリに持ち、変わったぶんを定期的にディスクへ落とす。落とす頻度を上げても
/// 得るものは無い (観測できるのは通りすがりの分だけで、失っても次のリクエストで
/// 拾い直せる) ので、日次集計と同じ周回に相乗りする。
pub struct QuotaStore {
    latest: RwLock<BTreeMap<CredentialId, Snapshot>>,
    /// 前回落としてから観測があったか。無ければ書かない。
    dirty: AtomicBool,
    /// 書き込み中であることの札。
    ///
    /// 定期の保存と終了時の保存が重なると、同じ一時ファイルを 2 者が切り詰め
    /// 合う経路が開く。書く側を 1 人に絞る ([`crate::stats::Stats`] と同じ)。
    writing: Mutex<()>,
    dir: PathBuf,
    /// このプロセスの書き込み先を他と分ける名前 (待ち受けポート)。
    writer: String,
}

impl QuotaStore {
    /// 置き場と書き手の名前を決めて作る。
    ///
    /// 前回の分を読み戻すのは呼び出し側 ([`Self::restore`])。
    pub fn new(dir: impl Into<PathBuf>, writer: &str) -> Self {
        Self {
            latest: RwLock::new(BTreeMap::new()),
            dirty: AtomicBool::new(false),
            writing: Mutex::new(()),
            dir: dir.into(),
            writer: sanitize_writer(writer),
        }
    }

    /// 応答ヘッダを通りすがりに読む。
    ///
    /// 転送のホットパスから呼ばれる。ヘッダを見て何も無ければ鍵も取らない。
    pub async fn observe(&self, id: &CredentialId, headers: &[(String, String)], now: i64) {
        let Some(snapshot) = Snapshot::from_headers(headers, now) else {
            return;
        };
        self.latest.write().await.insert(id.clone(), snapshot);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// この credential の最新スナップショット。まだ観測していなければ `None`。
    pub async fn get(&self, id: &CredentialId) -> Option<Snapshot> {
        self.latest.read().await.get(id).cloned()
    }

    /// 起動時に、自分が前回書いたファイルを読み戻す。
    ///
    /// 読めない中身は捨てて起動を続ける。ここで止まると、観測の控えという
    /// 付随的なもののために gateway 全体が上がらなくなる。
    ///
    /// serve の開始前に 1 回だけ呼ぶ前提。観測を始めた後に呼ぶと、読み戻した
    /// credential についてはメモリの値が古い値で置き換わる。
    pub async fn restore(&self) {
        sweep_temporaries(&self.dir, &self.writer);

        let path = self.path();
        if !path.exists() {
            return;
        }
        let saved: Saved = match std::fs::read_to_string(&path)
            .map_err(std::io::Error::other)
            .and_then(|raw| serde_json::from_str(&raw).map_err(std::io::Error::other))
        {
            Ok(saved) => saved,
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "利用状況を読めません");
                return;
            }
        };
        if saved.is_empty() {
            return;
        }

        let credentials = saved.len();
        *self.latest.write().await = saved
            .into_iter()
            .map(|(name, snapshot)| (CredentialId::new(name), snapshot))
            .collect();
        tracing::info!(credentials, "利用状況を読み戻しました");
    }

    /// 観測があればディスクへ落とす。無ければ何もしない。
    pub async fn save(&self) -> std::io::Result<()> {
        // 先に目印を外す。書いている間の観測は、次の周回で書き直される
        // (取りこぼしより書き直しの方が安い)。
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let saved: Saved = self
            .latest
            .read()
            .await
            .iter()
            .map(|(id, snapshot)| (id.to_string(), snapshot.clone()))
            .collect();

        // 書く者を 1 人に絞る。押さえている間に await しない。
        let _writing = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) =
            std::fs::create_dir_all(&self.dir).and_then(|()| write_atomically(&self.path(), &saved))
        {
            // 落とし損ねた分を次の周回へ回す。
            self.dirty.store(true, Ordering::Relaxed);
            return Err(e);
        }
        Ok(())
    }

    fn path(&self) -> PathBuf {
        self.dir.join(format!("usage-latest.{}.json", self.writer))
    }
}

/// その credential の利用状況をどこまで出せるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// 観測済み。スナップショットがある。
    Observed,
    /// 取れるはずだが、まだ観測していない (起動後この credential を使っていない)。
    Unobserved,
    /// 仕組みとして取れない。
    NotApplicable,
    /// 転送先の gateway 次第。
    UpstreamDependent,
}

/// credential 1 件分の報告。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialUsage {
    pub name: String,
    /// config.toml に書く type と同じ語。
    #[serde(rename = "type")]
    pub kind: String,
    pub support: Support,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    /// 専用の口から聞いた枠 ([`crate::limits`])。
    ///
    /// スナップショットと**別に持つ**。あちらは転送のついでに読んだ応答ヘッダ
    /// で、こちらは聞きに行った答え。同じ枠を指しているとは限らない (実測
    /// 2026-08-01: 同時刻に 7 日の枠がヘッダでは 0.34、この口では 100)。
    /// 聞けなかった credential では欠ける。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<Vec<Limit>>,
    /// 能動プローブが失敗した理由。他の credential は巻き添えにしない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

impl CredentialUsage {
    pub fn new(name: &str, kind: &str, support: Support, snapshot: Option<Snapshot>) -> Self {
        Self {
            name: name.to_owned(),
            kind: kind.to_owned(),
            support,
            snapshot,
            limits: None,
            probe_error: None,
        }
    }
}

/// 能動プローブの結果。
///
/// **usage の確認自体が usage を消費する**ので、何をどれだけ使ったかを
/// 出力に残す (DR-0007)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probe {
    /// 投げた credential の数。
    pub requests: usize,
    /// 使ったモデル。
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// 一括表示の中身。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub generated_at: i64,
    pub generated_at_iso: String,
    /// `?refresh=true` のときだけ入る。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<Probe>,
    pub credentials: Vec<CredentialUsage>,
}

impl Report {
    pub fn new(now: i64, credentials: Vec<CredentialUsage>) -> Self {
        Self {
            generated_at: now,
            generated_at_iso: format_rfc3339(now),
            probe: None,
            credentials,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-29T12:00:00Z
    const NOW: i64 = 1_785_326_400;

    fn usage(dir: &std::path::Path) -> QuotaStore {
        QuotaStore::new(dir, "8402")
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// 実測された unified ヘッダ一式 (DR-0007 の表)。
    fn unified() -> Vec<(String, String)> {
        headers(&[
            ("content-type", "application/json"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.71"),
            ("anthropic-ratelimit-unified-5h-reset", "1785344400"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.3"),
            ("anthropic-ratelimit-unified-7d-reset", "1785661200"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-overage-status", "disabled"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "out_of_credits",
            ),
        ])
    }

    #[test]
    fn reads_every_unified_field() {
        let s = Snapshot::from_headers(&unified(), NOW).expect("読める");

        assert_eq!(s.observed_at, NOW, "取得時刻を必ず付ける");
        assert_eq!(s.observed_at_iso, "2026-07-29T12:00:00Z");

        let five = s.five_hour.unwrap();
        assert_eq!(five.utilization, Some(0.71));
        assert_eq!(five.status.as_deref(), Some("allowed"));
        assert_eq!(five.reset, Some(1_785_344_400));
        assert_eq!(
            five.reset_iso.as_deref(),
            Some("2026-07-29T17:00:00Z"),
            "Unix 秒と ISO の両方を出す"
        );

        let seven = s.seven_day.unwrap();
        assert_eq!(seven.utilization, Some(0.3));
        assert_eq!(seven.reset, Some(1_785_661_200));

        let overage = s.overage.unwrap();
        assert_eq!(overage.status.as_deref(), Some("disabled"));
        assert_eq!(overage.disabled_reason.as_deref(), Some("out_of_credits"));
    }

    /// ヘッダ名の大小は問わない (upstream や手前のプロキシで変わりうる)。
    #[test]
    fn header_lookup_ignores_case() {
        let s = Snapshot::from_headers(
            &headers(&[("Anthropic-RateLimit-Unified-5h-Utilization", "0.5")]),
            NOW,
        )
        .unwrap();
        assert_eq!(s.five_hour.unwrap().utilization, Some(0.5));
    }

    /// undocumented なヘッダなので、欠けても壊れない。
    /// 読めた分だけ使い、残りは None のままにする。
    #[test]
    fn missing_fields_do_not_break_the_rest() {
        let s = Snapshot::from_headers(
            &headers(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.9"),
                (
                    "anthropic-ratelimit-unified-7d-utilization",
                    "beyond-repair",
                ),
            ]),
            NOW,
        )
        .unwrap();

        let five = s.five_hour.unwrap();
        assert_eq!(five.utilization, Some(0.9));
        assert_eq!(five.reset, None, "無いものは None");
        assert_eq!(five.reset_iso, None);
        assert!(s.overage.is_none());
        assert!(
            s.seven_day.is_none(),
            "1 つも読めなかった窓は、空の器を置かずに落とす"
        );
    }

    /// 利用状況が 1 つも無い応答からは作らない。
    ///
    /// 空のスナップショットを置くと「観測した」と「まだ観測していない」が
    /// 区別できなくなる。
    #[test]
    fn unrelated_response_yields_nothing() {
        assert!(
            Snapshot::from_headers(
                &headers(&[
                    ("content-type", "text/event-stream"),
                    ("anthropic-ratelimit-requests-remaining", "42"),
                ]),
                NOW,
            )
            .is_none()
        );
        assert!(Snapshot::from_headers(&[], NOW).is_none());
    }

    #[tokio::test]
    async fn latest_observation_wins() {
        let dir = tempfile::tempdir().unwrap();
        let usage = usage(dir.path());
        let id = CredentialId::new("claude-personal");

        assert_eq!(usage.get(&id).await, None, "使う前は未観測");

        usage.observe(&id, &unified(), NOW).await;
        assert_eq!(usage.get(&id).await.unwrap().observed_at, NOW);

        usage
            .observe(
                &id,
                &headers(&[("anthropic-ratelimit-unified-5h-utilization", "0.95")]),
                NOW + 600,
            )
            .await;
        let latest = usage.get(&id).await.unwrap();
        assert_eq!(latest.observed_at, NOW + 600);
        assert_eq!(latest.five_hour.unwrap().utilization, Some(0.95));
    }

    /// 関係ない応答は、前に観測した値を消さない。
    #[tokio::test]
    async fn unrelated_response_keeps_the_previous_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let usage = usage(dir.path());
        let id = CredentialId::new("c");

        usage.observe(&id, &unified(), NOW).await;
        usage
            .observe(
                &id,
                &headers(&[("content-type", "application/json")]),
                NOW + 60,
            )
            .await;

        assert_eq!(usage.get(&id).await.unwrap().observed_at, NOW);
    }

    /// credential ごとに別々に持つ。
    #[tokio::test]
    async fn snapshots_are_per_credential() {
        let dir = tempfile::tempdir().unwrap();
        let usage = usage(dir.path());
        let a = CredentialId::new("a");
        let b = CredentialId::new("b");

        usage.observe(&a, &unified(), NOW).await;
        assert!(usage.get(&a).await.is_some());
        assert!(usage.get(&b).await.is_none());
    }

    // ---------- 再起動を跨いで持つ ----------

    /// 落として読み戻すと、観測した値がそのまま返る。
    ///
    /// **取得時刻も一緒に戻る**のが肝。これが失われると、いつの値なのか
    /// 分からないものを最新として出すことになる。
    #[tokio::test]
    async fn a_snapshot_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let id = CredentialId::new("claude-personal");
        {
            let usage = usage(dir.path());
            usage.observe(&id, &unified(), NOW).await;
            usage.save().await.unwrap();
        }

        // 再起動に相当する。
        let usage = usage(dir.path());
        assert_eq!(usage.get(&id).await, None, "読み戻す前は未観測");
        usage.restore().await;

        let restored = usage.get(&id).await.expect("読み戻せる");
        assert_eq!(restored.observed_at, NOW, "取得時刻も戻る");
        assert_eq!(restored.observed_at_iso, "2026-07-29T12:00:00Z");
        assert_eq!(restored.five_hour.unwrap().utilization, Some(0.71));
        assert_eq!(
            restored.overage.unwrap().disabled_reason.as_deref(),
            Some("out_of_credits")
        );
    }

    /// 観測していなければ書かない。
    #[tokio::test]
    async fn nothing_is_written_without_an_observation() {
        let dir = tempfile::tempdir().unwrap();
        let usage = usage(dir.path());

        usage.save().await.unwrap();
        assert!(!usage.path().exists(), "空のファイルを置かない");

        // 利用状況の載っていない応答も観測ではない。
        usage
            .observe(
                &CredentialId::new("a"),
                &headers(&[("content-type", "application/json")]),
                NOW,
            )
            .await;
        usage.save().await.unwrap();
        assert!(!usage.path().exists());

        usage
            .observe(&CredentialId::new("a"), &unified(), NOW)
            .await;
        usage.save().await.unwrap();
        assert!(usage.path().exists(), "観測したら書く");
    }

    /// 変わっていなければ書き直さない。
    #[tokio::test]
    async fn an_unchanged_snapshot_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let usage = usage(dir.path());
        usage
            .observe(&CredentialId::new("a"), &unified(), NOW)
            .await;
        usage.save().await.unwrap();

        let first = std::fs::metadata(usage.path()).unwrap().modified().unwrap();
        usage.save().await.unwrap();
        let second = std::fs::metadata(usage.path()).unwrap().modified().unwrap();
        assert_eq!(first, second, "2 度目は触らない");
    }

    /// 壊れたファイルがあっても起動は続く。
    ///
    /// 観測の控えは付随的なもので、これで gateway が上がらないのは割に合わない。
    #[tokio::test]
    async fn a_broken_file_does_not_stop_the_startup() {
        let dir = tempfile::tempdir().unwrap();
        let broken = usage(dir.path());
        std::fs::write(broken.path(), "{ not json").unwrap();

        broken.restore().await;
        assert_eq!(broken.get(&CredentialId::new("a")).await, None);

        // 読めなかった後も、観測して書き直せる。
        broken
            .observe(&CredentialId::new("a"), &unified(), NOW)
            .await;
        broken.save().await.unwrap();

        let reopened = usage(dir.path());
        reopened.restore().await;
        assert!(reopened.get(&CredentialId::new("a")).await.is_some());
    }

    /// 他の writer のファイルは読まない。
    ///
    /// 向こうの観測は向こうが持っている。読むと、自分が書き戻すときに
    /// 相手の観測を自分のファイルへ写し取ってしまう。
    #[tokio::test]
    async fn another_writers_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        {
            let other = QuotaStore::new(dir.path(), "8401");
            other
                .observe(&CredentialId::new("a"), &unified(), NOW)
                .await;
            other.save().await.unwrap();
        }

        let usage = usage(dir.path());
        usage.restore().await;
        assert_eq!(usage.get(&CredentialId::new("a")).await, None);
    }

    /// 置き場が無くても読み戻しで落ちない (まだ 1 度も書いていない状態)。
    #[tokio::test]
    async fn a_missing_directory_restores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let usage = QuotaStore::new(dir.path().join("not-yet"), "8402");
        usage.restore().await;
        assert_eq!(usage.get(&CredentialId::new("a")).await, None);

        // 置き場は保存のときに作る。
        usage
            .observe(&CredentialId::new("a"), &unified(), NOW)
            .await;
        usage.save().await.unwrap();
        assert!(usage.path().exists());
    }

    /// 保存に失敗した分は、次の保存で書き直される。
    #[tokio::test]
    async fn a_failed_save_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        // 置き場と同じ名前のファイルを置いて、ディレクトリを作れなくする。
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();

        let usage = QuotaStore::new(&blocked, "8402");
        usage
            .observe(&CredentialId::new("a"), &unified(), NOW)
            .await;
        assert!(usage.save().await.is_err(), "書けないので失敗する");

        // 目印が残っているので、書ける状態になれば落ちる。
        std::fs::remove_file(&blocked).unwrap();
        usage.save().await.unwrap();
        assert!(usage.path().exists());
    }

    /// 待ち受け先がそのまま来ても、自分のファイルを読み戻せる。
    #[tokio::test]
    async fn a_listen_address_becomes_a_usable_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let id = CredentialId::new("a");
        {
            let usage = QuotaStore::new(dir.path(), "127.0.0.1:8402");
            usage.observe(&id, &unified(), NOW).await;
            usage.save().await.unwrap();
        }

        let usage = QuotaStore::new(dir.path(), "127.0.0.1:8402");
        usage.restore().await;
        assert!(usage.get(&id).await.is_some());
    }

    /// 前回の書き損じ (自分の一時ファイル) は起動時に片付ける。
    #[tokio::test]
    async fn leftover_temporaries_are_swept_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("usage-latest.8402.json.tmp.1234.0");
        let theirs = dir.path().join("usage-latest.8401.json.tmp.9999.0");
        std::fs::write(&mine, "{}").unwrap();
        std::fs::write(&theirs, "{}").unwrap();

        usage(dir.path()).restore().await;

        assert!(!mine.exists(), "自分の書き損じは消す");
        assert!(theirs.exists(), "他の writer の一時ファイルは触らない");
    }

    /// 日次集計と同じ置き場に置いても、互いのファイルを取り違えない。
    ///
    /// 日次集計は置き場を走査して日付ごとのファイルを足し合わせる。利用状況の
    /// ファイルがそこに紛れて日付として読まれると、ありえない日が一覧に出る。
    #[tokio::test]
    async fn the_file_is_not_mistaken_for_a_daily_aggregate() {
        let dir = tempfile::tempdir().unwrap();
        let usage = usage(dir.path());
        usage
            .observe(&CredentialId::new("a"), &unified(), NOW)
            .await;
        usage.save().await.unwrap();

        let stats = crate::stats::Stats::new(dir.path(), "8402");
        assert!(
            stats.report(0, NOW).days.is_empty(),
            "利用状況のファイルを日次集計として読まない"
        );
    }

    /// 未観測の credential も名前は出す (存在ごと消さない)。
    /// 取れない理由は support の値が担い、余計なフィールドは出さない。
    #[test]
    fn unobserved_credential_still_has_a_name() {
        let c = CredentialUsage::new("claude-work", "claude_oauth", Support::Unobserved, None);
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["name"], "claude-work");
        assert_eq!(json["type"], "claude_oauth");
        assert_eq!(json["support"], "unobserved");
        assert!(json.get("note").is_none(), "理由の文章は出さない");
        assert!(json.get("snapshot").is_none(), "無い値は出さない");
    }

    /// JSON の形。CLI はこれを読んで整形する。
    #[test]
    fn report_json_shape() {
        let report = Report::new(
            NOW,
            vec![CredentialUsage::new(
                "claude-personal",
                "claude_oauth",
                Support::Observed,
                Snapshot::from_headers(&unified(), NOW - 120),
            )],
        );
        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(json["generated_at"], NOW);
        assert_eq!(json["generated_at_iso"], "2026-07-29T12:00:00Z");
        assert!(json.get("probe").is_none(), "プローブ無しなら出さない");

        let c = &json["credentials"][0];
        assert_eq!(c["support"], "observed");
        assert_eq!(c["snapshot"]["5h"]["utilization"], 0.71);
        assert_eq!(c["snapshot"]["5h"]["reset"], 1_785_344_400_i64);
        assert_eq!(c["snapshot"]["5h"]["reset_iso"], "2026-07-29T17:00:00Z");
        assert_eq!(c["snapshot"]["7d"]["utilization"], 0.3);
        assert_eq!(
            c["snapshot"]["overage"]["disabled_reason"],
            "out_of_credits"
        );
        assert_eq!(c["snapshot"]["observed_at"], NOW - 120);
    }

    /// 読み書きで欠けない (CLI は書いた形をそのまま読む)。
    #[test]
    fn report_round_trips() {
        let mut report = Report::new(
            NOW,
            vec![CredentialUsage::new(
                "a",
                "claude_oauth",
                Support::Observed,
                Snapshot::from_headers(&unified(), NOW),
            )],
        );
        report.probe = Some(Probe {
            requests: 1,
            model: "claude-haiku-4-5-20251001".to_owned(),
            input_tokens: 8,
            output_tokens: 1,
        });

        let text = serde_json::to_string(&report).unwrap();
        let back: Report = serde_json::from_str(&text).unwrap();

        assert_eq!(back.credentials[0].support, Support::Observed);
        assert_eq!(back.credentials[0].snapshot, report.credentials[0].snapshot);
        assert_eq!(back.probe.unwrap().input_tokens, 8);
    }
}
