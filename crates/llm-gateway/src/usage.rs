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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::credential::CredentialId;
use crate::credential::time::format_rfc3339;

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

/// credential ごとの最新スナップショット。
///
/// メモリにだけ置く。再起動で消えるが、次のリクエストで拾い直せる
/// (永続化すると、消えない古い値を最新だと誤解する危険のほうが大きい)。
#[derive(Default)]
pub struct Usage {
    latest: RwLock<BTreeMap<CredentialId, Snapshot>>,
}

impl Usage {
    /// 応答ヘッダを通りすがりに読む。
    ///
    /// 転送のホットパスから呼ばれる。ヘッダを見て何も無ければ鍵も取らない。
    pub async fn observe(&self, id: &CredentialId, headers: &[(String, String)], now: i64) {
        let Some(snapshot) = Snapshot::from_headers(headers, now) else {
            return;
        };
        self.latest.write().await.insert(id.clone(), snapshot);
    }

    /// この credential の最新スナップショット。まだ観測していなければ `None`。
    pub async fn get(&self, id: &CredentialId) -> Option<Snapshot> {
        self.latest.read().await.get(id).cloned()
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

impl Support {
    /// なぜその扱いになるのか。表示側で毎回書き起こさずに済むよう、
    /// 理由はここを正本にする。
    pub fn note(self) -> Option<&'static str> {
        match self {
            Self::Observed => None,
            Self::Unobserved => Some("未観測 (この credential をまだ使っていません)"),
            Self::NotApplicable => Some("対象外 (AWS 課金)"),
            Self::UpstreamDependent => Some("転送先次第 (未対応)"),
        }
    }
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
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
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
            note: support.note().map(str::to_owned),
            snapshot,
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
        let usage = Usage::default();
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
        let usage = Usage::default();
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
        let usage = Usage::default();
        let a = CredentialId::new("a");
        let b = CredentialId::new("b");

        usage.observe(&a, &unified(), NOW).await;
        assert!(usage.get(&a).await.is_some());
        assert!(usage.get(&b).await.is_none());
    }

    /// 取れない理由は表示側で書き起こさない (ここが正本)。
    #[test]
    fn every_unsupported_case_explains_itself() {
        assert_eq!(Support::Observed.note(), None);
        assert!(Support::NotApplicable.note().unwrap().contains("AWS"));
        assert!(
            Support::UpstreamDependent
                .note()
                .unwrap()
                .contains("転送先")
        );
        assert!(Support::Unobserved.note().unwrap().contains("未観測"));
    }

    /// 未観測の credential も名前は出す (存在ごと消さない)。
    #[test]
    fn unobserved_credential_still_has_a_name() {
        let c = CredentialUsage::new("claude-work", "claude_oauth", Support::Unobserved, None);
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["name"], "claude-work");
        assert_eq!(json["type"], "claude_oauth");
        assert_eq!(json["support"], "unobserved");
        assert!(json["note"].as_str().unwrap().contains("未観測"));
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
