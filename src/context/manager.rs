//! Conversation context manager.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

use super::compact::compact_context;
use crate::llm::client::{BaseLlmClient, LlmRequestOptions};

/// Builder that rebuilds the stable system-message prefix when compaction
/// replaces the conversation. Equivalent to Python's `SystemMessagesBuilder`.
pub type SystemMessagesBuilder =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Vec<Value>>> + Send>> + Send + Sync>;

/// Cancellation flag used across compaction and long-running steps. Python
/// uses `threading.Event`; we use `Arc<Notify>` together with an atomic flag.
#[derive(Clone, Default)]
pub struct CancelEvent {
    flag: Arc<std::sync::atomic::AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelEvent {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn clear(&self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn wait_cancelled(&self) {
        if self.is_set() {
            return;
        }
        self.notify.notified().await;
    }
}

/// Manage conversation messages and compaction policy.
pub struct ConversationContextManager {
    messages: Vec<Value>,
    context_window_tokens: u32,
    compact_threshold: f64,
    current_tokens: u32,
    compact_trigger_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOutcome {
    Skipped,
    Compacted,
    Failed,
}

impl ConversationContextManager {
    pub fn new(
        init_messages: Vec<Value>,
        context_window_tokens: u32,
        compact_threshold: f64,
    ) -> Result<Self> {
        let window = context_window_tokens.max(1);
        let threshold = normalize_threshold(compact_threshold)?;
        let trigger = ((f64::from(window) * threshold).trunc() as u32).max(1);
        Ok(Self {
            messages: init_messages,
            context_window_tokens: window,
            compact_threshold: threshold,
            current_tokens: 0,
            compact_trigger_tokens: trigger,
        })
    }

    pub fn messages(&self) -> &[Value] {
        &self.messages
    }

    pub fn messages_mut(&mut self) -> &mut Vec<Value> {
        &mut self.messages
    }

    pub fn context_window_tokens(&self) -> u32 {
        self.context_window_tokens
    }

    pub fn compact_threshold(&self) -> f64 {
        self.compact_threshold
    }

    pub fn sync_token_usage(&mut self, total_tokens: u64) {
        self.current_tokens = u32::try_from(total_tokens).unwrap_or(u32::MAX);
    }

    pub fn should_compact(&self) -> bool {
        !self.messages.is_empty() && self.current_tokens >= self.compact_trigger_tokens
    }

    pub fn truncate_messages(&mut self, count: usize) {
        if count >= self.messages.len() {
            return;
        }
        self.messages.truncate(count);
    }

    pub fn add_user(&mut self, content: Value) -> Result<()> {
        let normalized = normalize_user_content(&content)?;
        self.messages
            .push(json!({"role": "user", "content": normalized}));
        Ok(())
    }

    pub fn add_assistant(&mut self, message: Value) {
        self.messages.push(message);
    }

    pub fn add_tool_result(&mut self, tool_call_id: &str, name: &str, output: &Value) {
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "name": name,
            "content": safe_json(output),
        }));
    }

    pub fn messages_for_model(&self, vision: bool) -> Vec<Value> {
        if vision {
            self.messages.clone()
        } else {
            self.messages
                .iter()
                .map(patch_message_for_text_model)
                .collect()
        }
    }

    /// Run compaction when the usage crosses the trigger, mirroring
    /// Python's `maybe_compact`.
    pub async fn maybe_compact(
        &mut self,
        model_client: &BaseLlmClient,
        cancel_event: Option<&CancelEvent>,
        rebuild_system_messages: Option<SystemMessagesBuilder>,
        reasoning_mode: Option<&str>,
        llm_options: LlmRequestOptions,
    ) -> CompactionOutcome {
        if self.messages.is_empty() {
            return CompactionOutcome::Skipped;
        }
        if self.current_tokens < self.compact_trigger_tokens {
            return CompactionOutcome::Skipped;
        }
        if let Some(ev) = cancel_event
            && ev.is_set()
        {
            return CompactionOutcome::Skipped;
        }

        let (mut stable_prefix, mut rest) = split_stable_prefix(&self.messages);
        if stable_prefix.is_empty() {
            if let Some(first) = self.messages.first() {
                stable_prefix = vec![first.clone()];
                rest = self.messages[1..].to_vec();
            }
        }
        if rest.is_empty() {
            return CompactionOutcome::Skipped;
        }

        let compact_input: Vec<Value> = if model_client.capabilities.vision {
            rest
        } else {
            rest.iter().map(patch_message_for_text_model).collect()
        };

        let compacted = match compact_context(
            &compact_input,
            model_client,
            cancel_event,
            reasoning_mode,
            llm_options,
        )
        .await
        {
            Ok(Some(messages)) => messages,
            Ok(None) => return CompactionOutcome::Skipped,
            Err(_) => return CompactionOutcome::Failed,
        };

        let mut next_prefix = stable_prefix;
        if let Some(builder) = rebuild_system_messages {
            if let Ok(rebuilt) = builder().await
                && !rebuilt.is_empty()
            {
                next_prefix = rebuilt
                    .into_iter()
                    .filter(|item| item.is_object())
                    .collect();
            }
        }

        if let Some(ev) = cancel_event
            && ev.is_set()
        {
            return CompactionOutcome::Skipped;
        }

        let mut merged: Vec<Value> = Vec::with_capacity(next_prefix.len() + compacted.len());
        merged.extend(next_prefix);
        merged.extend(compacted);
        self.messages = merged;
        self.current_tokens = 0;
        CompactionOutcome::Compacted
    }
}

fn normalize_threshold(value: f64) -> Result<f64> {
    if value <= 0.0 || value > 1.0 {
        bail!("compact_threshold must be in (0, 1]");
    }
    Ok(value)
}

fn safe_json(data: &Value) -> String {
    serde_json::to_string(data).unwrap_or_else(|_| {
        serde_json::to_string(&json!({ "result": format!("{data}") })).unwrap_or_default()
    })
}

fn split_stable_prefix(messages: &[Value]) -> (Vec<Value>, Vec<Value>) {
    let mut prefix: Vec<Value> = Vec::new();
    let mut index = 0;
    for item in messages {
        let role = item
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if role != "system" {
            break;
        }
        prefix.push(item.clone());
        index += 1;
    }
    (prefix, messages[index..].to_vec())
}

fn normalize_user_content(content: &Value) -> Result<Value> {
    match content {
        Value::String(s) => {
            let text = s.trim();
            if text.is_empty() {
                bail!("user_input cannot be empty");
            }
            Ok(Value::String(text.to_string()))
        }
        Value::Array(parts) => {
            if parts.is_empty() {
                bail!("user_input content blocks cannot be empty");
            }
            let mut out: Vec<Value> = Vec::with_capacity(parts.len());
            for (index, part) in parts.iter().enumerate() {
                out.push(normalize_user_part(part, index)?);
            }
            Ok(Value::Array(out))
        }
        _ => bail!("user_input must be string or list of content blocks"),
    }
}

fn normalize_user_part(part: &Value, index: usize) -> Result<Value> {
    let obj = part.as_object().ok_or_else(|| {
        anyhow::anyhow!("user_input content block at index {index} must be object")
    })?;

    let part_type = obj
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    match part_type {
        "text" | "input_text" => {
            let text = obj
                .get("text")
                .or_else(|| obj.get("input_text"))
                .cloned()
                .unwrap_or(Value::Null);
            let text_str = match text {
                Value::Null => String::new(),
                Value::String(s) => s,
                other => other.to_string(),
            };
            let trimmed = text_str.trim();
            if trimmed.is_empty() {
                bail!("text block at index {index} cannot be empty");
            }
            Ok(json!({"type": "text", "text": trimmed}))
        }
        "image_url" | "image" | "input_image" => normalize_image_part(obj, index),
        "resource" | "resource_link" => Ok(Value::Object(obj.clone())),
        other => bail!("Unsupported content block type at index {index}: {other}"),
    }
}

fn normalize_image_part(obj: &Map<String, Value>, index: usize) -> Result<Value> {
    let part_type = obj
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    if part_type == "image" {
        let data = obj
            .get("data")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !data.is_empty() {
            let mime_type = obj
                .get("mimeType")
                .or_else(|| obj.get("mime_type"))
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if mime_type.is_empty() {
                bail!("image block at index {index} must provide mimeType");
            }
            return Ok(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{mime_type};base64,{data}")},
            }));
        }
    }

    let url = extract_image_url(obj, index)?;
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

fn extract_image_url(obj: &Map<String, Value>, index: usize) -> Result<String> {
    let url_value = match obj.get("image_url") {
        Some(Value::Object(m)) => m.get("url").cloned().unwrap_or(Value::Null),
        Some(Value::String(s)) => Value::String(s.clone()),
        _ => obj
            .get("url")
            .or_else(|| obj.get("uri"))
            .cloned()
            .unwrap_or(Value::Null),
    };
    let url = match url_value {
        Value::Null => String::new(),
        Value::String(s) => s,
        other => other.to_string(),
    };
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("image block at index {index} must provide url");
    }
    Ok(trimmed.to_string())
}

fn patch_message_for_text_model(message: &Value) -> Value {
    let Some(obj) = message.as_object() else {
        return message.clone();
    };
    let mut payload = obj.clone();
    if let Some(Value::Array(parts)) = payload.get("content").cloned() {
        payload.insert(
            "content".to_string(),
            Value::Array(patch_content_parts_for_text_model(&parts)),
        );
    }
    Value::Object(payload)
}

fn patch_content_parts_for_text_model(parts: &[Value]) -> Vec<Value> {
    parts
        .iter()
        .map(|part| {
            let Some(obj) = part.as_object() else {
                return json!({"type": "text", "text": part.to_string()});
            };
            let part_type = obj
                .get("type")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if matches!(part_type, "image" | "image_url" | "input_image") {
                json!({"type": "text", "text": "[image]"})
            } else {
                Value::Object(obj.clone())
            }
        })
        .collect()
}
