//! upstream service の公式状態と実通信の観測を正規化する。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::{Config, StatusConfig, StatusSourceSpec};
use crate::credential::time::now_unix;

const MAX_BODY: usize = 1024 * 1024;
const MAX_UPDATE: usize = 4 * 1024;

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
    waiters: Vec<tokio::sync::oneshot::Sender<()>>,
    last_failure_trigger: Option<Instant>,
}
#[derive(Clone)]
struct Snapshot {
    at: i64,
    state: OfficialState,
    components: Vec<Component>,
    incidents: Vec<Incident>,
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
        for name in self.inner.sources.keys().cloned().collect::<Vec<_>>() {
            self.refresh_background(name);
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(this.inner.config.refresh_interval);
            t.tick().await;
            loop {
                t.tick().await;
                for n in this.inner.sources.keys().cloned().collect::<Vec<_>>() {
                    this.refresh_background(n);
                }
            }
        });
    }
    pub async fn refresh_all(&self) {
        let jobs = self.inner.sources.keys().cloned().map(|n| {
            let s = self.clone();
            async move { s.refresh(&n).await }
        });
        futures_util::future::join_all(jobs).await;
    }
    fn refresh_background(&self, name: String) {
        let s = self.clone();
        tokio::spawn(async move { s.refresh(&name).await });
    }
    async fn refresh(&self, name: &str) {
        let Some(source) = self.inner.sources.get(name) else {
            return;
        };
        let rx = {
            let mut st = source.state.lock().await;
            if st.refreshing {
                let (tx, rx) = tokio::sync::oneshot::channel();
                st.waiters.push(tx);
                Some(rx)
            } else {
                st.refreshing = true;
                None
            }
        };
        if let Some(rx) = rx {
            let _ = tokio::time::timeout(self.inner.config.request_timeout, rx).await;
            return;
        }
        let result = fetch(&source.spec, self.inner.config.request_timeout).await;
        let mut st = source.state.lock().await;
        match result {
            Ok(v) => {
                st.snapshot = Some(v);
                st.error = None
            }
            Err(e) => st.error = Some(e),
        };
        st.refreshing = false;
        for tx in st.waiters.drain(..) {
            let _ = tx.send(());
        }
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
                name: id,
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
        StatusSourceSpec::Link { page_url } => ("link", page_url.as_str()),
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

#[derive(Deserialize)]
struct Summary {
    status: RawStatus,
    components: Vec<RawComponent>,
}
#[derive(Deserialize)]
struct RawStatus {
    indicator: String,
}
#[derive(Deserialize)]
struct RawComponent {
    id: String,
    name: String,
    status: String,
}
#[derive(Deserialize)]
struct Incidents {
    incidents: Vec<RawIncident>,
}
#[derive(Deserialize)]
struct RawIncident {
    id: String,
    name: String,
    status: String,
    impact: String,
    created_at: String,
    updated_at: String,
    shortlink: String,
    #[serde(default)]
    incident_updates: Vec<Update>,
    #[serde(default)]
    components: Vec<RawComponent>,
}
#[derive(Deserialize)]
struct Update {
    body: String,
}
fn normalize(s: &str) -> OfficialState {
    match s {
        "none" | "operational" => OfficialState::Operational,
        "minor" | "degraded_performance" => OfficialState::Degraded,
        "partial_outage" => OfficialState::PartialOutage,
        "major" | "critical" | "major_outage" => OfficialState::MajorOutage,
        "maintenance" | "under_maintenance" => OfficialState::Maintenance,
        _ => OfficialState::Unknown,
    }
}
async fn bounded(client: &reqwest::Client, url: &url::Url) -> Result<Vec<u8>, String> {
    let resp = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if out.len() + chunk.len() > MAX_BODY {
            return Err("status response exceeds 1 MiB".into());
        }
        out.extend_from_slice(&chunk)
    }
    Ok(out)
}
async fn fetch(spec: &StatusSourceSpec, timeout: Duration) -> Result<Snapshot, String> {
    let StatusSourceSpec::StatuspageV2 {
        summary_url,
        incidents_url,
        components,
        ..
    } = spec
    else {
        return Ok(Snapshot {
            at: now_unix(),
            state: OfficialState::Unknown,
            components: vec![],
            incidents: vec![],
        });
    };
    let origin = summary_url.origin();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |a| {
            let u = a.url();
            if u.scheme() == "https" && u.origin() == origin {
                a.follow()
            } else {
                a.stop()
            }
        }))
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let (sb, ib) = tokio::try_join!(
        bounded(&client, summary_url),
        bounded(&client, incidents_url)
    )?;
    let summary: Summary =
        serde_json::from_slice(&sb).map_err(|e| format!("invalid status summary: {e}"))?;
    let incidents: Incidents =
        serde_json::from_slice(&ib).map_err(|e| format!("invalid incidents: {e}"))?;
    let selected: Vec<Component> = summary
        .components
        .into_iter()
        .filter(|c| components.is_empty() || components.contains(&c.name))
        .map(|c| Component {
            id: c.id,
            name: c.name,
            state: normalize(&c.status),
        })
        .collect();
    let state = selected
        .iter()
        .map(|c| c.state)
        .max_by_key(|s| match s {
            OfficialState::MajorOutage => 5,
            OfficialState::PartialOutage => 4,
            OfficialState::Degraded => 3,
            OfficialState::Maintenance => 2,
            OfficialState::Operational => 1,
            OfficialState::Unknown => 0,
        })
        .unwrap_or_else(|| normalize(&summary.status.indicator));
    let incidents = incidents
        .incidents
        .into_iter()
        .filter(|i| !matches!(i.status.as_str(), "resolved" | "postmortem"))
        .filter(|i| {
            i.components.is_empty()
                || i.components
                    .iter()
                    .any(|c| components.is_empty() || components.contains(&c.name))
        })
        .map(|i| {
            let mut latest = i
                .incident_updates
                .first()
                .map(|x| x.body.clone())
                .unwrap_or_default();
            while latest.len() > MAX_UPDATE {
                latest.pop();
            }
            Incident {
                id: i.id,
                name: i.name,
                state: i.status,
                impact: i.impact,
                created_at: i.created_at,
                updated_at: i.updated_at,
                url: i.shortlink,
                latest_update: latest,
            }
        })
        .collect();
    Ok(Snapshot {
        at: now_unix(),
        state,
        components: selected,
        incidents,
    })
}
