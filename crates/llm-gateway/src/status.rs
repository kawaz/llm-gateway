//! upstream service の公式状態と実通信の観測を正規化する。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::{Config, StatusConfig, StatusSourceSpec};
use crate::credential::time::now_unix;

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
    pub created_at: String,
    pub updated_at: String,
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
    pub observed_at: Option<i64>,
    pub stale: bool,
    pub components: Vec<Component>,
    pub incidents: Vec<Incident>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub at: i64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observed {
    pub state: ObservedState,
    pub observed_at: Option<i64>,
    pub expires_at: Option<i64>,
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
#[derive(Default, Clone)]
struct Observation {
    success: Option<i64>,
    failure: Option<Failure>,
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
            .filter(|(_, source)| matches!(source.spec, StatusSourceSpec::StatuspageV2 { .. }))
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
                let result = match &source.spec {
                    StatusSourceSpec::StatuspageV2 { .. } => {
                        use crate::statuspage_v2::StatusSource as _;
                        crate::statuspage_v2::Adapter::new(this.inner.config.request_timeout)
                            .fetch(&source.spec)
                            .await
                    }
                    StatusSourceSpec::Link { .. } => Ok(Snapshot {
                        at: now_unix(),
                        state: OfficialState::Unknown,
                        components: vec![],
                        incidents: vec![],
                    }),
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
    pub async fn observe_success(&self, route: &str) {
        self.inner
            .observations
            .lock()
            .await
            .entry(route.to_owned())
            .or_default()
            .success = Some(now_unix());
    }
    pub async fn observe_failure(&self, route: &str, kind: &str, status: Option<u16>) {
        let now = now_unix();
        self.inner
            .observations
            .lock()
            .await
            .entry(route.to_owned())
            .or_default()
            .failure = Some(Failure {
            at: now,
            kind: kind.to_owned(),
            status,
        });
        let Some(Some(source)) = self.inner.routes.get(route) else {
            return;
        };
        let Some(src) = self.inner.sources.get(source) else {
            return;
        };
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
        let now = now_unix();
        let obs = self.inner.observations.lock().await.clone();
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
                official_from(&id, &src.spec, &st, now, self.inner.config.stale_after)
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
            schema_version: 1,
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
    let mut success = None;
    let mut failure = None;
    for r in routes {
        if let Some(o) = all.get(r) {
            success = success.max(o.success);
            if o.failure
                .as_ref()
                .is_some_and(|f| failure.as_ref().is_none_or(|x: &Failure| f.at > x.at))
            {
                failure = o.failure.clone()
            }
        }
    }
    let latest = success.max(failure.as_ref().map(|f| f.at));
    let valid = latest.is_some_and(|x| now - x <= ttl.as_secs() as i64);
    let state = if !valid {
        ObservedState::Unknown
    } else if failure
        .as_ref()
        .is_some_and(|f| success.is_none_or(|s| f.at > s))
    {
        ObservedState::Failing
    } else {
        ObservedState::Reachable
    };
    Observed {
        state,
        observed_at: latest,
        expires_at: latest.map(|x| x + ttl.as_secs() as i64),
        last_success_at: success,
        last_failure: failure,
    }
}
fn official_from(
    id: &str,
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
            stale: now - x.at > stale_after.as_secs() as i64,
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
            error: st.error.clone().or_else(|| {
                if id.is_empty() {
                    Some("invalid source".into())
                } else {
                    None
                }
            }),
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
        let text = format!(
            r#"
[server]
listen = "127.0.0.1:0"
[status]
observation_ttl = "1s"
stale_after = "1s"
request_timeout = "100ms"
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
type = "link"
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
            at: now_unix() - 2,
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
                success: Some(now_unix() - 2),
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
        let jobs = (0..20)
            .map(|_| {
                let m = m.clone();
                tokio::spawn(async move { m.refresh("provider").await })
            })
            .collect::<Vec<_>>();
        let results = futures_util::future::join_all(jobs).await;
        assert!(results.into_iter().all(|r| r.unwrap() == Ok(())));
        arrived.acquire_many(2).await.unwrap().forget();
        tokio::task::yield_now().await;
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

    /// leader が request timeout 内に終わらない場合、待機者自身も同じ上限で終了する。
    #[tokio::test]
    async fn refresh_waiter_times_out() {
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
