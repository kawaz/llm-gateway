//! credential ごとの枠の使用状況 (DR-0007)。
//!
//! upstream は使用率を**応答ヘッダに載せて**返してくる。gateway は全ての
//! リクエストを仲介しているので、通りすがりに読んで credential ごとの最新
//! スナップショットを持てば、追加の API コールなしで一覧が作れる。
//!
//! **ヘッダの読み方は provider の知識**なので、ここには無い。core が規定するのは
//! 読んだ結果の形 ([`Snapshot`]) と、それを再起動を跨いで持つ置き場
//! ([`QuotaStore`]) だけ (DR-0014 §3)。写す仕事は各 preset の
//! [`crate::provider::Metering`] が担う。
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
use crate::persist::{sanitize_writer, sweep_temporaries, write_atomically};

/// provider の枠照会 API から得た枠 1 本。
///
/// `kind` とモデル名は upstream の語を保持する。core は期限と適用範囲を扱うが、
/// provider 固有の分類を別の固定 enum へ写し替えない。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaLimit {
    pub kind: String,
    pub percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub is_active: bool,
}

/// 1 つの窓 (5 時間 / 7 日) の状況。
///
/// どれも欠けうる。枠のヘッダは公式ドキュメントに記載が無く、予告なく
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
    /// 1 つも読めなかった窓か。空の器を置かずに落とすための判定。
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// リセット時刻を入れる。ISO 表記は Unix 秒から起こす。
    pub fn with_reset(mut self, reset: Option<i64>) -> Self {
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
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// ある時点で観測した枠の状況。
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
    /// 読めた分から組む。1 つも読めなければ `None`。
    ///
    /// `None` を返すのは大事で、ここで空のスナップショットを作ると
    /// 「観測した (中身は空)」と「まだ観測していない」が区別できなくなる。
    pub fn new(
        observed_at: i64,
        five_hour: Option<Window>,
        seven_day: Option<Window>,
        overage: Option<Overage>,
    ) -> Option<Self> {
        if five_hour.is_none() && seven_day.is_none() && overage.is_none() {
            return None;
        }
        Some(Self {
            observed_at,
            observed_at_iso: format_rfc3339(observed_at),
            five_hour,
            seven_day,
            overage,
        })
    }
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

    /// 経路が読み取ったスナップショットを控える。
    ///
    /// 読む仕事は provider が済ませている。ここは「誰の観測か」で束ねて
    /// 置くだけ。
    pub async fn observe(&self, id: &CredentialId, snapshot: Snapshot) {
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
                tracing::warn!(path = %path.display(), %e, "cannot read usage");
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
        tracing::info!(credentials, "loaded usage from disk");
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
    /// 枠照会 API ([`crate::provider::QuotaApi`]) から聞いた枠。
    ///
    /// スナップショットと**別に持つ**。あちらは転送のついでに読んだ応答ヘッダ
    /// で、こちらは聞きに行った答え。同じ枠を指しているとは限らない (実測
    /// 2026-08-01: 同時刻に 7 日の枠がヘッダでは 0.34、この口では 100)。
    /// 聞けなかった credential では欠ける。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<Vec<QuotaLimit>>,
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

    fn store(dir: &std::path::Path) -> QuotaStore {
        QuotaStore::new(dir, "8402")
    }

    /// 観測されたスナップショット 1 つ。provider が読んだ結果に相当する。
    fn observed(now: i64) -> Snapshot {
        Snapshot::new(
            now,
            Some(
                Window {
                    utilization: Some(0.71),
                    status: Some("allowed".to_owned()),
                    ..Window::default()
                }
                .with_reset(Some(1_785_344_400)),
            ),
            Some(
                Window {
                    utilization: Some(0.3),
                    status: Some("allowed".to_owned()),
                    ..Window::default()
                }
                .with_reset(Some(1_785_661_200)),
            ),
            Some(Overage {
                status: Some("disabled".to_owned()),
                disabled_reason: Some("out_of_credits".to_owned()),
            }),
        )
        .expect("something was read")
    }

    /// 1 つも読めなければスナップショットにしない。
    ///
    /// 空の器を置くと「観測した」と「まだ観測していない」が区別できなくなる。
    #[test]
    fn an_empty_reading_is_not_an_observation() {
        assert_eq!(Snapshot::new(NOW, None, None, None), None);
        assert!(Snapshot::new(NOW, Some(Window::default()), None, None).is_some());
    }

    /// 取得時刻は Unix 秒と ISO の両方を出す。
    #[test]
    fn a_snapshot_always_carries_its_time() {
        let s = observed(NOW);
        assert_eq!(s.observed_at, NOW);
        assert_eq!(s.observed_at_iso, "2026-07-29T12:00:00Z");
        assert_eq!(
            s.five_hour.unwrap().reset_iso.as_deref(),
            Some("2026-07-29T17:00:00Z"),
            "the window reset time is emitted in the same shape"
        );
    }

    #[tokio::test]
    async fn latest_observation_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let id = CredentialId::new("claude-personal");

        assert_eq!(store.get(&id).await, None, "unobserved before use");

        store.observe(&id, observed(NOW)).await;
        assert_eq!(store.get(&id).await.unwrap().observed_at, NOW);

        let later = Snapshot::new(
            NOW + 600,
            Some(Window {
                utilization: Some(0.95),
                ..Window::default()
            }),
            None,
            None,
        )
        .unwrap();
        store.observe(&id, later).await;

        let latest = store.get(&id).await.unwrap();
        assert_eq!(latest.observed_at, NOW + 600);
        assert_eq!(latest.five_hour.unwrap().utilization, Some(0.95));
    }

    /// credential ごとに別々に持つ。
    #[tokio::test]
    async fn snapshots_are_per_credential() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let a = CredentialId::new("a");
        let b = CredentialId::new("b");

        store.observe(&a, observed(NOW)).await;
        assert!(store.get(&a).await.is_some());
        assert!(store.get(&b).await.is_none());
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
            let store = store(dir.path());
            store.observe(&id, observed(NOW)).await;
            store.save().await.unwrap();
        }

        // 再起動に相当する。
        let store = store(dir.path());
        assert_eq!(store.get(&id).await, None, "unobserved before reload");
        store.restore().await;

        let restored = store.get(&id).await.expect("can be reloaded");
        assert_eq!(
            restored.observed_at, NOW,
            "the observed time is restored too"
        );
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
        let store = store(dir.path());

        store.save().await.unwrap();
        assert!(!store.path().exists(), "does not leave an empty file");

        store.observe(&CredentialId::new("a"), observed(NOW)).await;
        store.save().await.unwrap();
        assert!(store.path().exists(), "writes once observed");
    }

    /// 変わっていなければ書き直さない。
    #[tokio::test]
    async fn an_unchanged_snapshot_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.observe(&CredentialId::new("a"), observed(NOW)).await;
        store.save().await.unwrap();

        let first = std::fs::metadata(store.path()).unwrap().modified().unwrap();
        store.save().await.unwrap();
        let second = std::fs::metadata(store.path()).unwrap().modified().unwrap();
        assert_eq!(first, second, "the second time is untouched");
    }

    /// 壊れたファイルがあっても起動は続く。
    ///
    /// 観測の控えは付随的なもので、これで gateway が上がらないのは割に合わない。
    #[tokio::test]
    async fn a_broken_file_does_not_stop_the_startup() {
        let dir = tempfile::tempdir().unwrap();
        let broken = store(dir.path());
        std::fs::write(broken.path(), "{ not json").unwrap();

        broken.restore().await;
        assert_eq!(broken.get(&CredentialId::new("a")).await, None);

        // 読めなかった後も、観測して書き直せる。
        broken.observe(&CredentialId::new("a"), observed(NOW)).await;
        broken.save().await.unwrap();

        let reopened = store(dir.path());
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
            other.observe(&CredentialId::new("a"), observed(NOW)).await;
            other.save().await.unwrap();
        }

        let store = store(dir.path());
        store.restore().await;
        assert_eq!(store.get(&CredentialId::new("a")).await, None);
    }

    /// 置き場が無くても読み戻しで落ちない (まだ 1 度も書いていない状態)。
    #[tokio::test]
    async fn a_missing_directory_restores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = QuotaStore::new(dir.path().join("not-yet"), "8402");
        store.restore().await;
        assert_eq!(store.get(&CredentialId::new("a")).await, None);

        // 置き場は保存のときに作る。
        store.observe(&CredentialId::new("a"), observed(NOW)).await;
        store.save().await.unwrap();
        assert!(store.path().exists());
    }

    /// 保存に失敗した分は、次の保存で書き直される。
    #[tokio::test]
    async fn a_failed_save_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        // 置き場と同じ名前のファイルを置いて、ディレクトリを作れなくする。
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();

        let store = QuotaStore::new(&blocked, "8402");
        store.observe(&CredentialId::new("a"), observed(NOW)).await;
        assert!(store.save().await.is_err(), "fails because it cannot write");

        // 目印が残っているので、書ける状態になれば落ちる。
        std::fs::remove_file(&blocked).unwrap();
        store.save().await.unwrap();
        assert!(store.path().exists());
    }

    /// 待ち受け先がそのまま来ても、自分のファイルを読み戻せる。
    #[tokio::test]
    async fn a_listen_address_becomes_a_usable_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let id = CredentialId::new("a");
        {
            let store = QuotaStore::new(dir.path(), "127.0.0.1:8402");
            store.observe(&id, observed(NOW)).await;
            store.save().await.unwrap();
        }

        let store = QuotaStore::new(dir.path(), "127.0.0.1:8402");
        store.restore().await;
        assert!(store.get(&id).await.is_some());
    }

    /// 前回の書き損じ (自分の一時ファイル) は起動時に片付ける。
    #[tokio::test]
    async fn leftover_temporaries_are_swept_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("usage-latest.8402.json.tmp.1234.0");
        let theirs = dir.path().join("usage-latest.8401.json.tmp.9999.0");
        std::fs::write(&mine, "{}").unwrap();
        std::fs::write(&theirs, "{}").unwrap();

        store(dir.path()).restore().await;

        assert!(!mine.exists(), "removes its own failed write");
        assert!(theirs.exists(), "does not touch another writer's temp file");
    }

    /// 日次集計と同じ置き場に置いても、互いのファイルを取り違えない。
    ///
    /// 日次集計は置き場を走査して日付ごとのファイルを足し合わせる。利用状況の
    /// ファイルがそこに紛れて日付として読まれると、ありえない日が一覧に出る。
    #[tokio::test]
    async fn the_file_is_not_mistaken_for_a_daily_aggregate() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.observe(&CredentialId::new("a"), observed(NOW)).await;
        store.save().await.unwrap();

        /// 何も値付けしない役。ここで見たいのは日付の取り違えだけ。
        struct NoRates;

        impl crate::metering::PricingSource for NoRates {
            fn pricing(&self, _credential: &str, _model: &str) -> Option<crate::metering::Pricing> {
                None
            }
        }

        let stats = crate::stats::Stats::new(dir.path(), "8402");
        assert!(
            stats.report(0, NOW, &NoRates).days.is_empty(),
            "does not read the usage file as a daily aggregate"
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
        assert!(json.get("note").is_none(), "the reason text is omitted");
        assert!(json.get("snapshot").is_none(), "a missing value is omitted");
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
                Some(observed(NOW - 120)),
            )],
        );
        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(json["generated_at"], NOW);
        assert_eq!(json["generated_at_iso"], "2026-07-29T12:00:00Z");
        assert!(json.get("probe").is_none(), "omitted without a probe");

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
                Some(observed(NOW)),
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
