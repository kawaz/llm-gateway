//! Statuspage v2 JSON source adapter.

use std::time::Duration;

use futures_util::StreamExt as _;
use serde::Deserialize;

use crate::config::StatusSourceSpec;
use crate::credential::time::now_unix;
use crate::status::{Component, Incident, OfficialState, Snapshot};

const MAX_BODY: usize = 1024 * 1024;
const MAX_UPDATE: usize = 4 * 1024;

pub(crate) trait StatusSource {
    async fn fetch(&self, spec: &StatusSourceSpec) -> Result<Snapshot, String>;
}

pub(crate) struct Adapter {
    timeout: Duration,
}

impl Adapter {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self { timeout }
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
impl StatusSource for Adapter {
    async fn fetch(&self, spec: &StatusSourceSpec) -> Result<Snapshot, String> {
        let timeout = self.timeout;
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
        fn client_for(url: &url::Url, timeout: Duration) -> Result<reqwest::Client, String> {
            let origin = url.origin();
            reqwest::Client::builder()
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
                .map_err(|e| e.to_string())
        }
        let summary_client = client_for(summary_url, timeout)?;
        let incidents_client = client_for(incidents_url, timeout)?;
        let (sb, ib) = tokio::try_join!(
            bounded(&summary_client, summary_url),
            bounded(&incidents_client, incidents_url)
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
                    scope: i.components.is_empty().then(|| "page".to_owned()),
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
}
