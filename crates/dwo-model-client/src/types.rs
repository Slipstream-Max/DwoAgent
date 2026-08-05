use async_trait::async_trait;
use dwo_context::{CompactionView, ContextMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryReply {
    pub summary: String,
    pub usage: ModelUsage,
}

#[async_trait]
pub trait ModelClient: Send + Sync {
    fn model_limits(&self, model: &str) -> Result<ModelLimits, ModelClientError>;

    fn supports_image_input(&self, _model: &str) -> Result<bool, ModelClientError> {
        Ok(false)
    }

    /// Return the reasoning modes configured for a model, in any order.
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
