use std::collections::VecDeque;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, broadcast};

const HISTORY_LIMIT: usize = 1024;
const BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostEvent {
    pub seq: u64,
    pub event: String,
    pub params: Value,
}

struct EventState {
    next_seq: u64,
    history: VecDeque<HostEvent>,
}

pub(crate) struct HostEventHub {
    state: Mutex<EventState>,
    tx: broadcast::Sender<HostEvent>,
}

impl HostEventHub {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            state: Mutex::new(EventState {
                next_seq: 1,
                history: VecDeque::with_capacity(HISTORY_LIMIT),
            }),
            tx,
        }
    }

    pub(crate) async fn publish(&self, event: impl Into<String>, params: Value) -> HostEvent {
        let event = {
            let mut state = self.state.lock().await;
            let event = HostEvent {
                seq: state.next_seq,
                event: event.into(),
                params,
            };
            state.next_seq = state.next_seq.saturating_add(1);
            state.history.push_back(event.clone());
            if state.history.len() > HISTORY_LIMIT {
                state.history.pop_front();
            }
            event
        };
        let _ = self.tx.send(event.clone());
        event
    }

    pub(crate) async fn read(
        &self,
        cursor: Option<u64>,
        limit: usize,
        event_name: Option<&str>,
    ) -> EventReadResult {
        let limit = limit.clamp(1, 200);
        let state = self.state.lock().await;
        let start = cursor.unwrap_or(0);
        let oldest_cursor = state.history.front().map_or(start, |event| event.seq);
        let truncated = cursor.is_some_and(|value| value.saturating_add(1) < oldest_cursor);
        let events = state
            .history
            .iter()
            .filter(|event| event.seq > start)
            .filter(|event| event_name.is_none_or(|name| name == event.event))
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_cursor = events.last().map_or(start, |event| event.seq);
        EventReadResult {
            cursor: start,
            next_cursor,
            oldest_cursor,
            truncated,
            events,
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.tx.subscribe()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventReadResult {
    pub cursor: u64,
    pub next_cursor: u64,
    pub oldest_cursor: u64,
    pub truncated: bool,
    pub events: Vec<HostEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_have_monotonic_cursors_and_replay() {
        let hub = HostEventHub::new();
        hub.publish("config.changed", serde_json::json!({"source": "api"}))
            .await;
        hub.publish("mcp.status", serde_json::json!({"server": "demo"}))
            .await;

        let result = hub.read(Some(0), 10, None).await;
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].seq, 1);
        assert_eq!(result.events[1].seq, 2);
        assert_eq!(result.next_cursor, 2);
        assert!(!result.truncated);

        let filtered = hub.read(Some(0), 10, Some("mcp.status")).await;
        assert_eq!(filtered.events.len(), 1);
        assert_eq!(filtered.events[0].seq, 2);
    }

    #[tokio::test]
    async fn sustained_publish_load_has_bounded_history_and_runtime() {
        let hub = HostEventHub::new();
        let total = HISTORY_LIMIT * 4;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for index in 0..total {
                hub.publish("load.test", serde_json::json!({"index": index}))
                    .await;
            }
        })
        .await
        .expect("publishing bounded events took more than five seconds");

        let state = hub.state.lock().await;
        assert_eq!(state.history.len(), HISTORY_LIMIT);
        assert_eq!(
            state.history.front().unwrap().seq,
            (total - HISTORY_LIMIT + 1) as u64
        );
        assert_eq!(state.history.back().unwrap().seq, total as u64);
        drop(state);

        let replay = hub.read(Some(0), 200, None).await;
        assert!(replay.truncated);
        assert_eq!(replay.events.len(), 200);
        assert_eq!(replay.oldest_cursor, (total - HISTORY_LIMIT + 1) as u64);
    }
}
