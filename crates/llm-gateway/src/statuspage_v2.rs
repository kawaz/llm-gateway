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
                components.is_empty() || i.components.iter().any(|c| components.contains(&c.name))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    async fn server(summary: Vec<u8>, incidents: Vec<u8>, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let summary = Arc::new(summary);
        let incidents = Arc::new(incidents);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let summary = summary.clone();
                let incidents = incidents.clone();
                tokio::spawn(async move {
                    let mut request = [0; 1024];
                    let count = stream.read(&mut request).await.unwrap();
                    let path = String::from_utf8_lossy(&request[..count]);
                    let body = if path.starts_with("GET /summary ") {
                        &summary
                    } else {
                        &incidents
                    };
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(head.as_bytes()).await.unwrap();
                    stream.write_all(body).await.unwrap();
                });
            }
        });
        format!("http://{address}")
    }

    fn spec(base: &str, components: &[&str]) -> StatusSourceSpec {
        StatusSourceSpec::StatuspageV2 {
            name: Some("Provider".into()),
            summary_url: format!("{base}/summary").parse().unwrap(),
            incidents_url: format!("{base}/incidents").parse().unwrap(),
            page_url: format!("{base}/").parse().unwrap(),
            components: components.iter().map(|x| (*x).into()).collect(),
        }
    }

    fn summary(indicator: &str) -> Vec<u8> {
        format!(r#"{{"status":{{"indicator":"{indicator}"}},"components":[{{"id":"a","name":"API","status":"operational"}},{{"id":"w","name":"Web","status":"major_outage"}}]}}"#).into_bytes()
    }

    fn incidents(update: &str) -> Vec<u8> {
        format!(r#"{{"incidents":[{{"id":"page","name":"Page","status":"investigating","impact":"major","created_at":"c","updated_at":"u","shortlink":"p","incident_updates":[{{"body":"page"}}],"components":[]}},{{"id":"api","name":"API incident","status":"monitoring","impact":"minor","created_at":"c","updated_at":"u","shortlink":"a","incident_updates":[{{"body":{update:?}}}],"components":[{{"id":"a","name":"API","status":"degraded_performance"}}]}},{{"id":"web","name":"Web incident","status":"investigating","impact":"major","created_at":"c","updated_at":"u","shortlink":"w","components":[{{"id":"w","name":"Web","status":"major_outage"}}]}}]}}"#).into_bytes()
    }

    /// Statuspage の未知 indicator は operational と推測せず unknown に正規化する。
    #[tokio::test]
    async fn unknown_status_is_preserved_as_unknown() {
        let base = server(
            summary("new_state"),
            br#"{"incidents":[]}"#.to_vec(),
            Duration::ZERO,
        )
        .await;
        let snapshot = Adapter::new(Duration::from_secs(1))
            .fetch(&spec(&base, &["Missing"]))
            .await
            .unwrap();
        assert_eq!(snapshot.state, OfficialState::Unknown);
    }

    /// 壊れた JSON は snapshot を生成せず、summary の入力異常として返す。
    #[tokio::test]
    async fn malformed_summary_is_rejected() {
        let base = server(
            b"{".to_vec(),
            br#"{"incidents":[]}"#.to_vec(),
            Duration::ZERO,
        )
        .await;
        let error = Adapter::new(Duration::from_secs(1))
            .fetch(&spec(&base, &[]))
            .await
            .err()
            .expect("the invalid response must fail");
        assert!(error.starts_with("invalid status summary:"), "{error}");
    }

    /// 1 MiB を 1 byte でも超える response は全体を保持せず拒否する。
    #[tokio::test]
    async fn oversized_response_is_rejected() {
        let base = server(
            vec![b'x'; MAX_BODY + 1],
            br#"{"incidents":[]}"#.to_vec(),
            Duration::ZERO,
        )
        .await;
        let error = Adapter::new(Duration::from_secs(1))
            .fetch(&spec(&base, &[]))
            .await
            .err()
            .expect("the invalid response must fail");
        assert_eq!(error, "status response exceeds 1 MiB");
    }

    /// source が応答しない場合は adapter の request timeout 内で失敗する。
    #[tokio::test]
    async fn fetch_honors_the_request_timeout() {
        let base = server(
            summary("none"),
            br#"{"incidents":[]}"#.to_vec(),
            Duration::from_secs(1),
        )
        .await;
        let started = std::time::Instant::now();
        let result = Adapter::new(Duration::from_millis(20))
            .fetch(&spec(&base, &[]))
            .await;
        assert!(
            result.is_err(),
            "a request beyond the configured deadline must fail"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the adapter must not wait for the delayed server"
        );
    }

    /// filter が空なら page 全体を表し、page-scope と全 component incident を採用する。
    #[tokio::test]
    async fn empty_component_filter_uses_page_scope() {
        let base = server(summary("none"), incidents("ok"), Duration::ZERO).await;
        let snapshot = Adapter::new(Duration::from_secs(1))
            .fetch(&spec(&base, &[]))
            .await
            .unwrap();
        assert_eq!(snapshot.state, OfficialState::MajorOutage);
        assert_eq!(
            snapshot
                .incidents
                .iter()
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>(),
            ["page", "api", "web"]
        );
        assert_eq!(snapshot.incidents[0].scope.as_deref(), Some("page"));
    }

    /// filter が非空なら交差する component だけを採用し、page-scope や別 component の障害を混ぜない。
    #[tokio::test]
    async fn component_filter_keeps_only_intersecting_incidents() {
        let base = server(summary("major"), incidents("ok"), Duration::ZERO).await;
        let snapshot = Adapter::new(Duration::from_secs(1))
            .fetch(&spec(&base, &["API"]))
            .await
            .unwrap();
        assert_eq!(
            snapshot.state,
            OfficialState::Operational,
            "page indicator and Web outage do not raise API severity"
        );
        assert_eq!(
            snapshot
                .incidents
                .iter()
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>(),
            ["api"]
        );
    }

    /// latest update は UTF-8 を壊さず 4 KiB 以下へ切り詰める。
    #[tokio::test]
    async fn latest_update_truncates_on_a_utf8_character_boundary() {
        let update = "界".repeat(2000);
        let base = server(summary("none"), incidents(&update), Duration::ZERO).await;
        let snapshot = Adapter::new(Duration::from_secs(1))
            .fetch(&spec(&base, &["API"]))
            .await
            .unwrap();
        let latest = &snapshot.incidents[0].latest_update;
        assert!(latest.len() <= MAX_UPDATE);
        assert_eq!(latest, &update[..latest.len()]);
        assert!(latest.ends_with('界'));
    }
}
