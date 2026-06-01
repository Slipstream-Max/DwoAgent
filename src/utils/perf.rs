//! Opt-in performance trace helpers.

use std::env;

use serde_json::Value;

const TRACE_ENV: &str = "AgentService_TRACE_PERF";

fn perf_enabled() -> bool {
    let raw = env::var(TRACE_ENV).unwrap_or_default();
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub fn perf_log(event: &str, fields: &Value) {
    if !perf_enabled() {
        return;
    }
    tracing::info!(target: "perf", event = event, fields = %fields, "perf {event}");
}

fn text_size(value: &Value) -> usize {
    match value {
        Value::String(s) => s.chars().count(),
        Value::Array(items) => items.iter().map(text_size).sum(),
        Value::Object(map) => map.values().map(text_size).sum(),
        Value::Null => 0,
        other => other.to_string().chars().count(),
    }
}

pub fn messages_size(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|m| m.get("content").map(text_size).unwrap_or(0))
        .sum()
}
