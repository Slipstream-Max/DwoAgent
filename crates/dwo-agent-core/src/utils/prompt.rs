//! Helpers for prompt-shaped payloads.

use serde_json::Value;

/// Return the first non-empty text block from a string or list-of-blocks input.
pub fn extract_first_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Array(items) => {
            for item in items {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let ty = obj
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or_default();
                if ty != "text" {
                    continue;
                }
                let text = obj
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or_default();
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
            None
        }
        _ => None,
    }
}
