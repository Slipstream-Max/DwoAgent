use async_trait::async_trait;
use dwo_context::{CompactionView, ContextMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ModelClientError;

#[derive(Debug, Clone)]
pub struct ModelSelection {
    pub model: String,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelLimits {
    pub context_window_tokens: u64,
    pub max_output_tokens: u32,
    pub max_input_tokens: u64,
    pub compact_trigger_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall(StreamToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_input: Value,
    pub status: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelReply {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_tool_calls: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_items: Vec<Value>,
    pub finish_reason: FinishReason,
    pub usage: ModelUsage,
}

impl ModelReply {
    /// Return item-first Responses output. The fallback keeps lightweight test
    /// clients and custom implementations on the same context contract.
    pub fn context_output_items(&self) -> Vec<Value> {
        if !self.output_items.is_empty() {
            return self.output_items.clone();
        }
        let mut items = Vec::new();
        if let Some(reasoning) = self.reasoning.as_ref().filter(|text| !text.is_empty()) {
            items.push(json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": reasoning}],
            }));
        }
        if !self.content.is_empty() {
            items.push(json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": self.content}],
            }));
        }
        items.extend(self.tool_calls.iter().map(|call| {
            let arguments = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
            json!({
                "type": "function_call",
                "call_id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                "name": call.get("name").and_then(Value::as_str).unwrap_or_default(),
                "arguments": match arguments {
                    Value::String(value) => value,
                    value => value.to_string(),
                },
            })
        }));
        items
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryReply {
    pub summary: String,
    pub usage: ModelUsage,
}

#[async_trait]
pub trait ModelClient: Send + Sync {
    fn model_limits(&self, model: &str) -> Result<ModelLimits, ModelClientError>;

    /// Stable configured provider-instance identity for context ownership.
    fn provider_id(&self, _model: &str) -> Result<String, ModelClientError> {
        Ok("default".to_string())
    }

    fn supports_image_input(&self, _model: &str) -> Result<bool, ModelClientError> {
        Ok(false)
    }

    /// Return the reasoning modes configured for a model, in configuration order.
    ///
    /// Clients that do not expose a model catalog may return an empty list;
    /// callers should then use the provider's implicit default.
    fn reasoning_modes(&self, _model: &str) -> Result<Vec<String>, ModelClientError> {
        Ok(Vec::new())
    }

    fn validate_selection(&self, _selection: &ModelSelection) -> Result<(), ModelClientError> {
        Ok(())
    }

    async fn stream_turn(
        &self,
        selection: ModelSelection,
        messages: &[ContextMessage],
        tools: &[Value],
        events: mpsc::UnboundedSender<ModelStreamEvent>,
        cancellation: &CancellationToken,
    ) -> Result<ModelReply, ModelClientError>;

    async fn complete(
        &self,
        selection: ModelSelection,
        messages: Vec<ContextMessage>,
        cancellation: CancellationToken,
    ) -> Result<ModelReply, ModelClientError>;

    async fn summarize(
        &self,
        selection: ModelSelection,
        view: CompactionView,
        cancellation: CancellationToken,
    ) -> Result<SummaryReply, ModelClientError>;
}
