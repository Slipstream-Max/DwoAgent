//! Conversation context manager.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::compact::compact_context;
use super::content_block::{image_placeholder, is_image_part, normalize_user_content};
use crate::config::models::ContextUsageSnapshot;
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

    pub fn usage_snapshot(&self) -> ContextUsageSnapshot {
        ContextUsageSnapshot {
            used: u64::from(self.current_tokens),
            size: u64::from(self.context_window_tokens),
        }
    }

    pub fn is_over_context_window(&self) -> bool {
        self.current_tokens >= self.context_window_tokens
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

    pub fn add_watcher_content(&mut self, message: Value) {
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

    pub fn messages_for_model(&self, vision: bool) -> Cow<'_, [Value]> {
        if vision {
            Cow::Borrowed(&self.messages)
        } else {
            Cow::Owned(
                self.messages
                    .iter()
                    .map(patch_message_for_text_model)
                    .collect(),
            )
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
    let Some(first) = messages.first() else {
        return (Vec::new(), Vec::new());
    };
    let role = first
        .get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if role == "system" {
        (vec![first.clone()], messages[1..].to_vec())
    } else {
        (Vec::new(), messages.to_vec())
    }
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
            if is_image_part(part) {
                image_placeholder()
            } else {
                Value::Object(obj.clone())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_user_stores_plain_text_as_typed_content_block() {
        let mut manager = ConversationContextManager::new(Vec::new(), 1000, 0.8).unwrap();

        manager.add_user(json!("hello")).unwrap();

        assert_eq!(
            manager.messages()[0]["content"],
            json!([{"type": "text", "text": "hello"}])
        );
    }

    #[test]
    fn messages_for_text_model_replaces_image_parts_and_keeps_text_path() {
        let mut manager = ConversationContextManager::new(Vec::new(), 1000, 0.8).unwrap();
        manager
            .add_user(json!([
                {"type": "text", "text": "收到图片文件：C:/tmp/image.png"},
                {"type": "image", "data": "abc", "mimeType": "image/png"}
            ]))
            .unwrap();

        let text_messages = manager.messages_for_model(false);
        let vision_messages = manager.messages_for_model(true);

        assert_eq!(
            text_messages[0]["content"],
            json!([
                {"type": "text", "text": "收到图片文件：C:/tmp/image.png"},
                {"type": "text", "text": "该处为图片消息。"}
            ])
        );
        assert_eq!(
            vision_messages[0]["content"],
            json!([
                {"type": "text", "text": "收到图片文件：C:/tmp/image.png"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
            ])
        );
    }

    #[test]
    fn only_first_system_message_is_stable_prefix() {
        let messages = vec![
            json!({"role": "system", "content": "stable"}),
            json!({"role": "system", "content": "<watcher_content>\n<env_block></env_block>\n</watcher_content>"}),
            json!({"role": "user", "content": "hello"}),
        ];
        let (prefix, rest) = split_stable_prefix(&messages);

        assert_eq!(prefix.len(), 1);
        assert_eq!(rest.len(), 2);
    }
}
