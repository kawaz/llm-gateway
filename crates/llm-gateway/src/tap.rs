//! 購読中だけ転送の詳細を流すデバッグ用 tap (DR-0017)。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

const BACKLOG: usize = 64;
pub const DEFAULT_MAX_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub request_body: bool,
    pub response_body: bool,
    pub max_body: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturePlan {
    pub request_body: usize,
    pub response_body: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub ts: i64,
    pub ns: String,
    pub model: String,
    pub route: String,
    pub status: u16,
    pub thinking: Option<Value>,
    pub tool_choice: Option<String>,
    pub stream: bool,
    pub request_body_size: usize,
    pub response_body_size: usize,
    pub credential: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

struct State {
    next_id: usize,
    subscriptions: BTreeMap<usize, Options>,
}

pub struct Tap {
    tx: broadcast::Sender<Event>,
    watchers: AtomicUsize,
    state: Mutex<State>,
}

impl Default for Tap {
    fn default() -> Self {
        Self::new()
    }
}

impl Tap {
    pub fn new() -> Self {
        Self {
            tx: broadcast::Sender::new(BACKLOG),
            watchers: AtomicUsize::new(0),
            state: Mutex::new(State {
                next_id: 0,
                subscriptions: BTreeMap::new(),
            }),
        }
    }

    pub fn subscribe(self: &Arc<Self>, options: Options) -> Subscription {
        let mut state = self.state.lock().unwrap();
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.subscriptions.insert(id, options);
        self.watchers.fetch_add(1, Ordering::Release);
        let receiver = self.tx.subscribe();
        Subscription {
            tap: Arc::clone(self),
            id,
            options,
            receiver,
        }
    }

    /// 購読者がいなければ lock もシリアライズも行わない。
    pub fn capture_plan(&self) -> Option<CapturePlan> {
        if self.watchers.load(Ordering::Acquire) == 0 {
            return None;
        }
        let state = self.state.lock().unwrap();
        let mut plan = CapturePlan {
            request_body: 0,
            response_body: 0,
        };
        for options in state.subscriptions.values() {
            if options.request_body {
                plan.request_body = plan.request_body.max(options.max_body);
            }
            if options.response_body {
                plan.response_body = plan.response_body.max(options.max_body);
            }
        }
        Some(plan)
    }

    pub fn publish(&self, event: Event) {
        if self.watchers.load(Ordering::Acquire) == 0 {
            return;
        }
        let _ = self.tx.send(event);
    }

    pub fn watchers(&self) -> usize {
        self.watchers.load(Ordering::Acquire)
    }
}

pub struct Subscription {
    tap: Arc<Tap>,
    id: usize,
    options: Options,
    receiver: broadcast::Receiver<Event>,
}

impl Subscription {
    pub async fn recv(&mut self) -> Result<Event, broadcast::error::RecvError> {
        let mut event = self.receiver.recv().await?;
        if !self.options.request_body {
            event.request_body = None;
        } else if let Some(body) = event.request_body.as_mut() {
            truncate_string(body, self.options.max_body);
        }
        if !self.options.response_body {
            event.response_body = None;
        } else if let Some(body) = event.response_body.as_mut() {
            truncate_string(body, self.options.max_body);
        }
        Ok(event)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.tap
            .state
            .lock()
            .unwrap()
            .subscriptions
            .remove(&self.id);
        self.tap.watchers.fetch_sub(1, Ordering::Release);
    }
}

pub fn capture(bytes: &[u8], max: usize) -> Option<String> {
    (max > 0).then(|| String::from_utf8_lossy(&bytes[..bytes.len().min(max)]).into_owned())
}

fn truncate_string(value: &mut String, max: usize) {
    if value.len() <= max {
        return;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Event {
        Event {
            ts: 1,
            ns: "personal".into(),
            model: "m".into(),
            route: "r".into(),
            status: 200,
            thinking: None,
            tool_choice: None,
            stream: false,
            request_body_size: 6,
            response_body_size: 6,
            credential: None,
            request_body: Some("abcdef".into()),
            response_body: Some("uvwxyz".into()),
        }
    }

    #[test]
    fn nobody_watching_skips_capture_planning() {
        let tap = Tap::new();
        assert_eq!(tap.watchers(), 0);
        assert_eq!(tap.capture_plan(), None);
        tap.publish(event());
    }

    #[tokio::test]
    async fn every_subscriber_receives_the_exchange() {
        let tap = Arc::new(Tap::new());
        let mut a = tap.subscribe(Options::default());
        let mut b = tap.subscribe(Options::default());
        tap.publish(event());
        assert_eq!(a.recv().await.unwrap().status, 200);
        assert_eq!(b.recv().await.unwrap().status, 200);
    }

    #[tokio::test]
    async fn bodies_are_opt_in_and_truncated_per_subscriber() {
        let tap = Arc::new(Tap::new());
        let mut metadata = tap.subscribe(Options::default());
        let mut bodies = tap.subscribe(Options {
            request_body: true,
            response_body: true,
            max_body: 3,
        });
        assert_eq!(
            tap.capture_plan(),
            Some(CapturePlan {
                request_body: 3,
                response_body: 3
            })
        );
        tap.publish(event());
        let metadata = metadata.recv().await.unwrap();
        assert_eq!(metadata.request_body, None);
        assert_eq!(metadata.response_body, None);
        let bodies = bodies.recv().await.unwrap();
        assert_eq!(bodies.request_body.as_deref(), Some("abc"));
        assert_eq!(bodies.response_body.as_deref(), Some("uvw"));
    }
}
