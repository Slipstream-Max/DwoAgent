//! Shared channel rendering for agent activity updates.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::agent::activity::event::{
    EVENT_AGENT_MESSAGE_CHUNK, EVENT_AGENT_THOUGHT_CHUNK, EVENT_TOOL_CALL, update_type,
};
use crate::tools::UpdateEmitter;

const TOOL_ARG_STRING_LIMIT: usize = 220;
const TOOL_ARG_COLLECTION_LIMIT: usize = 24;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelResponseDetail {
    ResponseOnly,
    Detailed,
}

impl Default for ChannelResponseDetail {
    fn default() -> Self {
        Self::ResponseOnly
    }
}

impl ChannelResponseDetail {
    fn includes_process(self) -> bool {
        matches!(self, Self::Detailed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelCollectedUpdates {
    pub detail_text: Option<String>,
    pub response_text: String,
}

#[derive(Debug, Default)]
struct ChannelUpdateState {
    response_text: String,
    thought_text: String,
    tool_calls: Vec<String>,
}

#[derive(Clone)]
pub struct ChannelUpdateCollector {
    detail: ChannelResponseDetail,
    state: Arc<Mutex<ChannelUpdateState>>,
}

impl ChannelUpdateCollector {
    pub fn new(detail: ChannelResponseDetail) -> Self {
        Self {
            detail,
            state: Arc::new(Mutex::new(ChannelUpdateState::default())),
        }
    }

    pub fn emitter(&self) -> UpdateEmitter {
        let detail = self.detail;
        let state = self.state.clone();
        Arc::new(move |_target: String, update: Map<String, Value>| {
            let state = state.clone();
            Box::pin(async move {
                state.lock().await.record_update(detail, &update);
                Ok(())
            })
        })
    }

    pub async fn finish(&self) -> ChannelCollectedUpdates {
        self.state.lock().await.finish(self.detail)
    }
}

impl ChannelUpdateState {
    fn record_update(&mut self, detail: ChannelResponseDetail, update: &Map<String, Value>) {
        match update_type(update) {
            EVENT_AGENT_MESSAGE_CHUNK => {
                if let Some(text) = update_text(update) {
                    self.response_text.push_str(text);
                }
            }
            EVENT_AGENT_THOUGHT_CHUNK if detail.includes_process() => {
                if let Some(text) = update_text(update) {
                    self.thought_text.push_str(text);
                }
            }
            EVENT_TOOL_CALL if detail.includes_process() => {
                if let Some(rendered) = render_tool_call(update) {
                    self.tool_calls.push(rendered);
                }
            }
            _ => {}
        }
    }

    fn finish(&self, detail: ChannelResponseDetail) -> ChannelCollectedUpdates {
        let detail_text = detail
            .includes_process()
            .then(|| {
                let mut sections = Vec::new();
                if !self.thought_text.trim().is_empty() {
                    sections.push(format!("[thinking]\n{}", self.thought_text));
                }
                if !self.tool_calls.is_empty() {
                    sections.push(format!("[tool_call]\n{}", self.tool_calls.join("\n")));
                }
                sections.join("\n\n")
            })
            .filter(|text| !text.trim().is_empty());

        ChannelCollectedUpdates {
            detail_text,
            response_text: self.response_text.trim().to_string(),
        }
    }
}

fn update_text(update: &Map<String, Value>) -> Option<&str> {
    update
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
}

fn render_tool_call(update: &Map<String, Value>) -> Option<String> {
    let title = update
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())?;
    let Some(raw_input) = update.get("raw_input") else {
        return Some(title.to_string());
    };
    if raw_input.as_object().is_some_and(Map::is_empty) {
        return Some(title.to_string());
    }
    Some(format!("{title} {}", render_tool_args(raw_input)))
}

fn render_tool_args(raw_input: &Value) -> String {
    let trimmed = truncate_json_value(raw_input);
    serde_json::to_string(&trimmed).unwrap_or_else(|_| "<unrenderable>".to_string())
}

fn truncate_json_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(truncate_chars(text, TOOL_ARG_STRING_LIMIT)),
        Value::Array(items) => {
            let mut out: Vec<Value> = items
                .iter()
                .take(TOOL_ARG_COLLECTION_LIMIT)
                .map(truncate_json_value)
                .collect();
            if items.len() > TOOL_ARG_COLLECTION_LIMIT {
                out.push(json!(format!(
                    "...<{} more items>",
                    items.len() - TOOL_ARG_COLLECTION_LIMIT
                )));
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (index, (key, value)) in map.iter().enumerate() {
                if index >= TOOL_ARG_COLLECTION_LIMIT {
                    out.insert(
                        "...".to_string(),
                        json!(format!(
                            "...<{} more entries>",
                            map.len() - TOOL_ARG_COLLECTION_LIMIT
                        )),
                    );
                    break;
                }
                out.insert(key.clone(), truncate_json_value(value));
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            out.push_str("...<truncated>");
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn response_only_collects_final_response_only() {
        let collector = ChannelUpdateCollector::new(ChannelResponseDetail::ResponseOnly);
        let emit = collector.emitter();

        emit("session".to_string(), thought_update("hidden"))
            .await
            .unwrap();
        emit(
            "session".to_string(),
            tool_update("terminal_exec", json!({"command": "cargo check"})),
        )
        .await
        .unwrap();
        emit("session".to_string(), message_update("final"))
            .await
            .unwrap();

        let collected = collector.finish().await;
        assert_eq!(collected.detail_text, None);
        assert_eq!(collected.response_text, "final");
    }

    #[tokio::test]
    async fn detailed_collects_thinking_and_truncated_tool_calls() {
        let collector = ChannelUpdateCollector::new(ChannelResponseDetail::Detailed);
        let emit = collector.emitter();
        let long_patch = "x".repeat(TOOL_ARG_STRING_LIMIT + 10);

        emit("session".to_string(), thought_update("think all of this"))
            .await
            .unwrap();
        emit(
            "session".to_string(),
            tool_update("file_edit", json!({"patch": long_patch})),
        )
        .await
        .unwrap();
        emit("session".to_string(), message_update("complete response"))
            .await
            .unwrap();

        let collected = collector.finish().await;
        let detail = collected.detail_text.unwrap();
        assert!(detail.contains("[thinking]\nthink all of this"));
        assert!(detail.contains("[tool_call]\nfile_edit"));
        assert!(detail.contains("...<truncated>"));
        assert_eq!(collected.response_text, "complete response");
    }

    fn message_update(text: &str) -> Map<String, Value> {
        json!({
            "session_update": EVENT_AGENT_MESSAGE_CHUNK,
            "content": {"type": "text", "text": text}
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn thought_update(text: &str) -> Map<String, Value> {
        json!({
            "session_update": EVENT_AGENT_THOUGHT_CHUNK,
            "content": {"type": "text", "text": text}
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn tool_update(title: &str, raw_input: Value) -> Map<String, Value> {
        json!({
            "session_update": EVENT_TOOL_CALL,
            "title": title,
            "raw_input": raw_input
        })
        .as_object()
        .cloned()
        .unwrap()
    }
}
