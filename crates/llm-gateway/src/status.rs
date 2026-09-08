//! upstream service の公式状態と実通信の観測を正規化する。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::{Config, StatusConfig, StatusSourceSpec};
use crate::credential::time::now_unix_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficialState {
    Operational,
    Degraded,
    PartialOutage,
    MajorOutage,
    Maintenance,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedState {
    Reachable,
    Failing,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Unknown,
    Ok,
    Warning,
    Critical,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub id: String,
    pub name: String,
    pub state: OfficialState,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub name: String,
    pub state: String,
    pub impact: String,
    /// 障害が立った時刻 (Unix ミリ秒)。upstream の表記が読めなければ欄ごと出さない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    /// 最後に更新された時刻 (Unix ミリ秒)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    pub url: String,
    pub latest_update: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Official {
    pub state: OfficialState,
    pub source: String,
    pub source_url: String,
    /// 公式値を取りに行けた時刻 (Unix ミリ秒)。
    pub observed_at: Option<i64>,
    pub stale: bool,
    pub components: Vec<Component>,
    pub incidents: Vec<Incident>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    /// 失敗を観測した時刻 (Unix ミリ秒)。
    pub at: i64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observed {
    pub state: ObservedState,
    /// 最後に何かを観測した時刻 (Unix ミリ秒)。
    pub observed_at: Option<i64>,
    /// その観測を現在状態として扱わなくなる時刻 (Unix ミリ秒)。
    pub expires_at: Option<i64>,
    /// 最後に通った時刻 (Unix ミリ秒)。
    pub last_success_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<Failure>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub severity: Severity,
    pub routes: Vec<String>,
    pub official: Official,
    pub observed: Observed,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    pub ok: usize,
    pub warning: usize,
    pub critical: usize,
    pub unknown: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overall {
    pub severity: Severity,
    pub service_counts: Counts,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u8,
    /// この報告を組んだ時刻 (Unix ミリ秒)。
    pub generated_at: i64,
    pub overall: Overall,
    pub services: Vec<Service>,
}

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}
struct Inner {
    config: StatusConfig,
    routes: BTreeMap<String, Option<String>>,
    sources: BTreeMap<String, Source>,
    observations: Mutex<BTreeMap<String, Observation>>,
    /// 観測の到着順。同じミリ秒に届いた成功と失敗は時刻だけでは並べられない。
    /// どちらが後かは通し番号で決める。
    sequence: AtomicU64,
}
struct Source {
    spec: StatusSourceSpec,
    state: Mutex<SourceState>,
}
#[derive(Default)]
struct SourceState {
    snapshot: Option<Snapshot>,
    error: Option<String>,
    refreshing: bool,
    waiters: Vec<tokio::sync::oneshot::Sender<Result<(), String>>>,
    last_failure_trigger: Option<Instant>,
}
#[derive(Clone)]
pub(crate) struct Snapshot {
    pub(crate) at: i64,
    pub(crate) state: OfficialState,
    pub(crate) components: Vec<Component>,
    pub(crate) incidents: Vec<Incident>,
}
/// 1 つの観測が「いつ」「何番目に」届いたか。
#[derive(Clone, Copy)]
struct Stamp {
    at: i64,
    seq: u64,
}
#[derive(Default, Clone)]
struct Observation {
    success: Option<Stamp>,
    failure: Option<(Stamp, Failure)>,
}

impl Manager {
    pub fn new(config: &Config) -> Self {
        let routes = config
            .routes
            .iter()
            .map(|(n, r)| (n.clone(), r.status_source.clone()))
            .collect();
        let sources = config
            .status
            .sources
            .iter()
            .map(|(n, s)| {
                (
                    n.clone(),
                    Source {
                        spec: s.clone(),
                        state: Mutex::new(SourceState::default()),
                    },
                )
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                config: config.status.clone(),
                routes,
                sources,
                observations: Mutex::new(BTreeMap::new()),
                sequence: AtomicU64::new(0),
            }),
        }
    }
    pub fn start(&self) {
        for name in self.fetchable_source_names() {
            self.refresh_background(name);
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(this.inner.config.refresh_interval);
            t.tick().await;
            loop {
                t.tick().await;
                for n in this.fetchable_source_names() {
                    this.refresh_background(n);
                }
            }
        });
    }
    pub async fn refresh_all(&self) {
        let jobs = self.fetchable_source_names().into_iter().map(|n| {
            let s = self.clone();
            async move { s.refresh(&n).await }
        });
        futures_util::future::join_all(jobs).await;
    }
    fn fetchable_source_names(&self) -> Vec<String> {
        self.inner
            .sources
            .iter()
            .filter(|(_, source)| is_fetchable(&source.spec))
            .map(|(name, _)| name.clone())
            .collect()
    }
    fn refresh_background(&self, name: String) {
        let s = self.clone();
        tokio::spawn(async move {
            let _ = s.refresh(&name).await;
        });
    }
    async fn refresh(&self, name: &str) -> Result<(), String> {
        let Some(source) = self.inner.sources.get(name) else {
            return Err(format!("unknown status source: {name}"));
        };
        // `link` は外部へ触らない案内先なので、更新しても新しく分かることが
        // 無い。取得した振りの snapshot を置くと、時刻だけが進んで公式値の
        // 鮮度 (`stale`) を計算する対象になってしまう。
        if !is_fetchable(&source.spec) {
            return Ok(());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let start = {
            let mut st = source.state.lock().await;
            st.waiters.push(tx);
            if st.refreshing {
                false
            } else {
                st.refreshing = true;
                true
            }
        };
        if start {
            let this = self.clone();
            let name = name.to_owned();
            tokio::spawn(async move {
                let source = this
                    .inner
                    .sources
                    .get(&name)
                    .expect("the source exists for the lifetime of the manager");
                let result = {
                    use crate::statuspage_v2::StatusSource as _;
                    crate::statuspage_v2::Adapter::new(this.inner.config.request_timeout)
                        .fetch(&source.spec)
                        .await
                };
                let mut st = source.state.lock().await;
                match &result {
                    Ok(v) => {
                        st.snapshot = Some(v.clone());
                        st.error = None;
                    }
                    Err(e) => st.error = Some(e.clone()),
                }
                st.refreshing = false;
                for tx in st.waiters.drain(..) {
                    let _ = tx.send(result.clone().map(|_| ()));
                }
            });
        }
        tokio::time::timeout(self.inner.config.request_timeout, rx)
            .await
            .map_err(|_| "status refresh timed out".to_owned())?
            .map_err(|_| "status refresh was cancelled".to_owned())?
    }
    fn stamp(&self) -> Stamp {
        Stamp {
            at: now_unix_ms(),
            seq: self.inner.sequence.fetch_add(1, Ordering::Relaxed),
        }
    }
    pub async fn observe_success(&self, route: &str) {
        let stamp = self.stamp();
        self.inner
            .observations
            .lock()
            .await
            .entry(route.to_owned())
            .or_default()
            .success = Some(stamp);
    }
    pub async fn observe_failure(&self, route: &str, kind: &str, status: Option<u16>) {
        let stamp = self.stamp();
        self.inner
            .observations
            .lock()
            .await
            .entry(route.to_owned())
            .or_default()
            .failure = Some((
            stamp,
            Failure {
                at: stamp.at,
                kind: kind.to_owned(),
                status,
            },
        ));
        let Some(Some(source)) = self.inner.routes.get(route) else {
            return;
        };
        let Some(src) = self.inner.sources.get(source) else {
            return;
        };
        if !is_fetchable(&src.spec) {
            return;
        }
        let trigger = {
            let mut st = src.state.lock().await;
            let due = st
                .last_failure_trigger
                .is_none_or(|x| x.elapsed() >= self.inner.config.failure_refresh_cooldown);
            if due {
                st.last_failure_trigger = Some(Instant::now())
            }
            due
        };
        if trigger {
            self.refresh_background(source.clone())
        }
    }
    pub async fn report(&self) -> Report {
        let now = now_unix_ms();
        let obs = self.inner.observations.lock().await.clone();
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        // 設定した source は、まだどの route も指していなくても service として
        // 残す。消してしまうと「書いたのに出てこない」だけになり、`routes` が
        // 空であること自体が書き落としの手がかりにならない。
        for name in self.inner.sources.keys() {
            groups.entry(name.clone()).or_default();
        }
        for (r, s) in &self.inner.routes {
            groups
                .entry(s.clone().unwrap_or_else(|| r.clone()))
                .or_default()
                .push(r.clone())
        }
        let mut services = Vec::new();
        for (id, routes) in groups {
            let official = if let Some(src) = self.inner.sources.get(&id) {
                let st = src.state.lock().await;
                official_from(&src.spec, &st, now, self.inner.config.stale_after)
            } else {
                Official {
                    state: OfficialState::Unknown,
                    source: "none".into(),
                    source_url: "".into(),
                    observed_at: None,
                    stale: false,
                    components: vec![],
                    incidents: vec![],
                    error: None,
                }
            };
            let observed = observed_from(&routes, &obs, now, self.inner.config.observation_ttl);
            let severity = severity(official.state, observed.state);
            services.push(Service {
                id: id.clone(),
                name: self
                    .inner
                    .sources
                    .get(&id)
                    .and_then(|source| source.spec.name())
                    .unwrap_or(&id)
                    .to_owned(),
                routes,
                severity,
                official,
                observed,
            });
        }
        let mut counts = Counts::default();
        for s in &services {
            match s.severity {
                Severity::Ok => counts.ok += 1,
                Severity::Warning => counts.warning += 1,
                Severity::Critical => counts.critical += 1,
                Severity::Unknown => counts.unknown += 1,
            }
        }
        let overall = services
            .iter()
            .map(|s| s.severity)
            .max()
            .unwrap_or(Severity::Unknown);
        Report {
            schema_version: 2,
            generated_at: now,
            overall: Overall {
                severity: overall,
                service_counts: counts,
            },
            services,
        }
    }
}
fn severity(o: OfficialState, x: ObservedState) -> Severity {
    if x == ObservedState::Failing || o == OfficialState::MajorOutage {
        Severity::Critical
    } else if matches!(
        o,
        OfficialState::Degraded | OfficialState::PartialOutage | OfficialState::Maintenance
    ) {
        Severity::Warning
    } else if o == OfficialState::Operational || x == ObservedState::Reachable {
        Severity::Ok
    } else {
        Severity::Unknown
    }
}
fn observed_from(
    routes: &[String],
    all: &BTreeMap<String, Observation>,
    now: i64,
    ttl: Duration,
) -> Observed {
    let mut success: Option<Stamp> = None;
    let mut failure: Option<(Stamp, Failure)> = None;
    for r in routes {
        if let Some(o) = all.get(r) {
            if o.success
                .is_some_and(|s| success.is_none_or(|x| s.seq > x.seq))
            {
                success = o.success
            }
            if o.failure
                .as_ref()
                .is_some_and(|(s, _)| failure.as_ref().is_none_or(|(x, _)| s.seq > x.seq))
            {
                failure = o.failure.clone()
            }
        }
    }
    let latest = success
        .map(|s| s.at)
        .max(failure.as_ref().map(|(s, _)| s.at));
    let valid = latest.is_some_and(|x| now - x <= ttl.as_millis() as i64);
    // 順序は到着順で決める。時刻だけで比べると、同じ目盛りに届いた失敗が成功へ
    // 埋もれる (逆は埋もれない) という向きの偏りが出る。
    let state = if !valid {
        ObservedState::Unknown
    } else if failure
        .as_ref()
        .is_some_and(|(f, _)| success.is_none_or(|s| f.seq > s.seq))
    {
        ObservedState::Failing
    } else {
        ObservedState::Reachable
    };
    Observed {
        state,
        observed_at: latest,
        expires_at: latest.map(|x| x + ttl.as_millis() as i64),
        last_success_at: success.map(|s| s.at),
        last_failure: failure.map(|(_, f)| f),
    }
}
fn is_fetchable(spec: &StatusSourceSpec) -> bool {
    matches!(spec, StatusSourceSpec::StatuspageV2 { .. })
}
fn official_from(
    spec: &StatusSourceSpec,
    st: &SourceState,
    now: i64,
    stale_after: Duration,
) -> Official {
    let (kind, url) = match spec {
        StatusSourceSpec::StatuspageV2 { page_url, .. } => ("statuspage_v2", page_url.as_str()),
        StatusSourceSpec::Link { page_url, .. } => ("link", page_url.as_str()),
    };
    match &st.snapshot {
        Some(x) => Official {
            state: x.state,
            source: kind.into(),
            source_url: url.into(),
            observed_at: Some(x.at),
            stale: now - x.at > stale_after.as_millis() as i64,
            components: x.components.clone(),
            incidents: x.incidents.clone(),
            error: st.error.clone(),
        },
        None => Official {
            state: OfficialState::Unknown,
            source: kind.into(),
            source_url: url.into(),
            observed_at: None,
            stale: false,
            components: vec![],
            incidents: vec![],
            error: st.error.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    async fn counting_status_server() -> (String, Arc<AtomicUsize>, Arc<tokio::sync::Semaphore>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let arrived = Arc::new(tokio::sync::Semaphore::new(0));
        let server_hits = hits.clone();
        let server_arrived = arrived.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                server_hits.fetch_add(1, Ordering::SeqCst);
                server_arrived.add_permits(1);
                tokio::spawn(async move {
                    let mut request = [0; 1024];
                    let count = stream.read(&mut request).await.unwrap();
                    let path = String::from_utf8_lossy(&request[..count]);
                    let body = if path.starts_with("GET /summary ") {
                        r#"{"status":{"indicator":"none"},"components":[]}"#
                    } else {
                        r#"{"incidents":[]}"#
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        (format!("http://{address}"), hits, arrived)
    }

    fn manager(extra: &str) -> Manager {
        manager_with_timeout(extra, "5s")
    }

    fn manager_with_timeout(extra: &str, request_timeout: &str) -> Manager {
        let text = format!(
            r#"
[server]
listen = "127.0.0.1:0"
[status]
observation_ttl = "1s"
stale_after = "1s"
request_timeout = "{request_timeout}"
failure_refresh_cooldown = "60s"
{extra}
"#
        );
        let config: Config = toml::from_str(&text).expect("the status fixture is valid");
        Manager::new(&config)
    }

    /// source を持たない route も service として残し、公式値を捏造せず実測だけを示す。
    #[tokio::test]
    async fn a_route_without_a_source_is_its_own_unknown_service() {
        let m = manager(
            r#"
[routes.direct]
provider = "anthropic"
models = ["m"]
"#,
        );
        m.observe_success("direct").await;
        let report = m.report().await;
        let service = report.services.iter().find(|s| s.id == "direct").unwrap();
        assert_eq!(service.routes, ["direct"]);
        assert_eq!(service.official.state, OfficialState::Unknown);
        assert_eq!(service.official.source, "none");
        assert_eq!(service.observed.state, ObservedState::Reachable);
        assert_eq!(service.severity, Severity::Ok);
    }

    /// 公式取得失敗後も最後の成功 snapshot を保持し、error と stale を独立して公開する。
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_last_successful_snapshot() {
        let m = manager(
            r#"
[status.sources.provider]
type = "statuspage_v2"
summary_url = "https://status.example/api/v2/summary.json"
incidents_url = "https://status.example/api/v2/incidents.json"
page_url = "https://status.example/"
[routes.route]
provider = "anthropic"
status_source = "provider"
models = ["m"]
"#,
        );
        let source = m.inner.sources.get("provider").unwrap();
        let mut state = source.state.lock().await;
        state.snapshot = Some(Snapshot {
            at: now_unix_ms() - 2_000,
            state: OfficialState::Operational,
            components: vec![],
            incidents: vec![],
        });
        state.error = Some("refresh failed".into());
        drop(state);
        let service = &m.report().await.services[0];
        assert_eq!(service.official.state, OfficialState::Operational);
        assert!(service.official.stale);
        assert_eq!(service.official.error.as_deref(), Some("refresh failed"));
    }

    /// 529 だけが failing を作り、後続成功は同秒でも reachable へ戻す。
    #[tokio::test]
    async fn success_after_a_529_restores_reachability() {
        let m = manager(
            r#"
[routes.route]
provider = "anthropic"
models = ["m"]
"#,
        );
        m.observe_failure("route", "overloaded", Some(529)).await;
        assert_eq!(
            m.report().await.services[0].observed.state,
            ObservedState::Failing
        );
        m.observe_success("route").await;
        assert_eq!(
            m.report().await.services[0].observed.state,
            ObservedState::Reachable
        );
    }

    /// TTL より古い実測は成功・失敗のどちらも現在状態として扱わない。
    #[tokio::test]
    async fn observations_become_unknown_after_the_ttl() {
        let m = manager(
            r#"
[routes.route]
provider = "anthropic"
models = ["m"]
"#,
        );
        m.inner.observations.lock().await.insert(
            "route".into(),
            Observation {
                success: Some(Stamp {
                    at: now_unix_ms() - 2_000,
                    seq: 0,
                }),
                failure: None,
            },
        );
        assert_eq!(
            m.report().await.services[0].observed.state,
            ObservedState::Unknown
        );
    }

    /// 同時 refresh は leader の summary/incidents 各 1 request だけを送り、全 waiter が同じ成功結果を受け取る。
    #[tokio::test]
    async fn concurrent_refreshes_share_one_fetch_and_result() {
        let (base, hits, _arrived) = counting_status_server().await;
        let m = manager(&format!(
            r#"
[status.sources.provider]
type = "statuspage_v2"
summary_url = "{base}/summary"
incidents_url = "{base}/incidents"
page_url = "{base}/"
[routes.route]
provider = "anthropic"
status_source = "provider"
models = ["m"]
"#
        ));
        // task へ切り出さず 1 つの future として畳むのは、20 本すべてが待ち行列へ
        // 並んでから leader の取得が終わる順序を保証するため。別 task にすると
        // 先に終わった取得の後から並ぶ組が出て、その分だけ request が増える。
        let results = futures_util::future::join_all((0..20).map(|_| m.refresh("provider"))).await;
        assert!(results.into_iter().all(|r| r == Ok(())));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "one summary and one incidents request form the single fetch"
        );
    }

    /// configured component が summary に無い場合は page 全体の障害を流用せず、unknown と取得エラーを公開する。
    #[tokio::test]
    async fn missing_configured_components_report_unknown_with_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut request = [0; 1024];
                    let count = stream.read(&mut request).await.unwrap();
                    let path = String::from_utf8_lossy(&request[..count]);
                    let body = if path.starts_with("GET /summary ") {
                        r#"{"status":{"indicator":"major"},"components":[]}"#
                    } else {
                        r#"{"incidents":[]}"#
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        let base = format!("http://{address}");
        let m = manager(&format!(
            r#"
[status.sources.provider]
type = "statuspage_v2"
summary_url = "{base}/summary"
incidents_url = "{base}/incidents"
page_url = "{base}/"
components = ["API"]
[routes.route]
provider = "anthropic"
status_source = "provider"
models = ["m"]
"#
        ));

        assert_eq!(
            m.refresh("provider").await,
            Err("configured components not found".into())
        );
        let official = &m.report().await.services[0].official;
        assert_eq!(official.state, OfficialState::Unknown);
        assert_eq!(
            official.error.as_deref(),
            Some("configured components not found")
        );
    }

    /// refresh 呼び出し元が中断されても独立した fetch は完走し、refreshing を解除して後続へ結果を渡す。
    #[tokio::test]
    async fn cancelled_refresh_leader_does_not_poison_single_flight() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let arrived = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let server_arrived = arrived.clone();
        let server_release = release.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let arrived = server_arrived.clone();
                let release = server_release.clone();
                tokio::spawn(async move {
                    let mut request = [0; 1024];
                    let count = stream.read(&mut request).await.unwrap();
                    let path = String::from_utf8_lossy(&request[..count]);
                    let body = if path.starts_with("GET /summary ") {
                        r#"{"status":{"indicator":"none"},"components":[]}"#
                    } else {
                        r#"{"incidents":[]}"#
                    };
                    arrived.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        let base = format!("http://{address}");
        let m = manager(&format!(
            r#"
[status.sources.provider]
type = "statuspage_v2"
summary_url = "{base}/summary"
incidents_url = "{base}/incidents"
page_url = "{base}/"
[routes.route]
provider = "anthropic"
status_source = "provider"
models = ["m"]
"#
        ));

        let leader = {
            let m = m.clone();
            tokio::spawn(async move { m.refresh("provider").await })
        };
        arrived.acquire_many(2).await.unwrap().forget();
        leader.abort();
        release.add_permits(2);

        assert_eq!(m.refresh("provider").await, Ok(()));
        assert_eq!(
            m.report().await.services[0].official.state,
            OfficialState::Operational
        );
    }

    /// failure が一度に大量到着しても source ごとの cooldown は最初の background refresh だけを起動する。
    #[tokio::test]
    async fn many_failures_trigger_one_refresh_during_the_cooldown() {
        let (base, hits, arrived) = counting_status_server().await;
        let m = manager(&format!(
            r#"
[status.sources.provider]
type = "statuspage_v2"
summary_url = "{base}/summary"
incidents_url = "{base}/incidents"
page_url = "{base}/"
[routes.route]
provider = "anthropic"
status_source = "provider"
models = ["m"]
"#
        ));
        let jobs = (0..100).map(|_| {
            let m = m.clone();
            tokio::spawn(
                async move { m.observe_failure("route", "upstream_http", Some(529)).await },
            )
        });
        futures_util::future::join_all(jobs).await;
        arrived.acquire_many(2).await.unwrap().forget();
        tokio::task::yield_now().await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "cooldown permits one summary and one incidents request only"
        );
    }

    /// 同じミリ秒に届いた失敗は、直前の成功へ埋もれず failing として残る。
    ///
    /// 時刻の一致は実時計に任せず観測を直接植える — 2 回の `now` が同じ目盛りに
    /// 落ちるかは運で、境界を跨ぐと通し番号の出番が無いまま通ってしまう。
    #[tokio::test]
    async fn a_failure_in_the_same_millisecond_outranks_the_earlier_success() {
        let m = manager(
            r#"
[routes.route]
provider = "anthropic"
models = ["m"]
"#,
        );
        let at = now_unix_ms();
        {
            let mut observations = m.inner.observations.lock().await;
            let observation = observations.entry("route".to_owned()).or_default();
            observation.success = Some(Stamp { at, seq: 0 });
            observation.failure = Some((
                Stamp { at, seq: 1 },
                Failure {
                    at,
                    kind: "upstream_http".to_owned(),
                    status: Some(529),
                },
            ));
        }
        let observed = &m.report().await.services[0].observed;
        assert_eq!(observed.state, ObservedState::Failing);
        assert_eq!(
            observed.last_success_at, observed.observed_at,
            "同じ目盛りなら成功も失敗も同じ時刻を持つ"
        );
    }

    /// `link` は取得しないので snapshot を作らず、公式値は鮮度の計算対象にならない。
    #[tokio::test]
    async fn a_link_source_never_becomes_stale() {
        let m = manager(
            r#"
[status.sources.provider]
type = "link"
page_url = "https://status.example/"
[routes.route]
provider = "anthropic"
status_source = "provider"
models = ["m"]
"#,
        );
        assert_eq!(m.refresh("provider").await, Ok(()));
        m.observe_failure("route", "upstream_http", Some(529)).await;
        let official = &m.report().await.services[0].official;
        assert_eq!(official.source, "link");
        assert_eq!(official.state, OfficialState::Unknown);
        assert!(!official.stale);
        assert_eq!(official.observed_at, None);
    }

    /// どの route からも指されていない source も、route が空の service として残る。
    #[tokio::test]
    async fn a_source_without_routes_stays_in_the_report() {
        let m = manager(
            r#"
[status.sources.unused]
type = "link"
page_url = "https://status.example/"
[routes.direct]
provider = "anthropic"
models = ["m"]
"#,
        );
        let report = m.report().await;
        let service = report.services.iter().find(|s| s.id == "unused").unwrap();
        assert!(service.routes.is_empty());
        assert_eq!(service.official.source, "link");
        assert_eq!(service.severity, Severity::Unknown);
    }

    /// leader が request timeout 内に終わらない場合、待機者自身も同じ上限で終了する。
    #[tokio::test]
    async fn refresh_waiter_times_out() {
        let m = manager_with_timeout(
            r#"
[status.sources.provider]
type = "statuspage_v2"
summary_url = "https://status.example/api/v2/summary.json"
incidents_url = "https://status.example/api/v2/incidents.json"
page_url = "https://status.example/"
[routes.route]
provider = "anthropic"
status_source = "provider"
models = ["m"]
"#,
            "100ms",
        );
        m.inner
            .sources
            .get("provider")
            .unwrap()
            .state
            .lock()
            .await
            .refreshing = true;
        assert_eq!(
            m.refresh("provider").await,
            Err("status refresh timed out".into())
        );
    }
}
