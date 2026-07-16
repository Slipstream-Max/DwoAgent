use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dwo_context::{CompactionView, ContextMessage};
use dwo_model_client::{
    FinishReason, ModelClient, ModelClientError, ModelLimits, ModelReply, ModelSelection,
    ModelStreamEvent, ModelUsage, SummaryReply,
};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct RecordedTurnRequest {
    pub selection: ModelSelection,
    pub messages: Vec<ContextMessage>,
}

#[derive(Debug, Clone)]
pub struct RecordedSummaryRequest {
    pub selection: ModelSelection,
    pub view: CompactionView,
}

#[derive(Debug, Clone)]
pub struct RecordedCompletionRequest {
    pub selection: ModelSelection,
    pub messages: Vec<ContextMessage>,
}

#[derive(Debug, Clone)]
pub enum ScriptedStep {
    Response {
        chunks: Vec<String>,
        tool_calls: Vec<Value>,
        finish_reason: FinishReason,
        delay_ms: u64,
        input_tokens: u64,
        output_tokens: u64,
    },
    ReasoningResponse {
        reasoning: String,
        content: String,
    },
    ContextLengthExceeded,
}

#[derive(Debug, Clone)]
pub struct ScriptedSummaryStep {
    pub summary: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ScriptedCompletionStep {
    pub content: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl ScriptedStep {
    pub fn text(content: impl Into<String>) -> Self {
        Self::Response {
            chunks: vec![content.into()],
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            delay_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    pub fn tools(chunks: Vec<String>, tool_calls: Vec<Value>) -> Self {
        Self::Response {
            chunks,
            tool_calls,
            finish_reason: FinishReason::ToolCalls,
            delay_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    pub fn delayed_text(content: impl Into<String>, delay_ms: u64) -> Self {
        Self::Response {
            chunks: vec![content.into()],
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            delay_ms,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    pub fn reasoning_text(reasoning: impl Into<String>, content: impl Into<String>) -> Self {
        Self::ReasoningResponse {
            reasoning: reasoning.into(),
            content: content.into(),
        }
    }
}

pub struct ScriptedModelGateway {
    steps: Mutex<VecDeque<ScriptedStep>>,
    completion_steps: Mutex<VecDeque<ScriptedCompletionStep>>,
    summary_steps: Mutex<VecDeque<ScriptedSummaryStep>>,
    requests: Mutex<Vec<RecordedTurnRequest>>,
    completion_requests: Mutex<Vec<RecordedCompletionRequest>>,
    summary_requests: Mutex<Vec<RecordedSummaryRequest>>,
    default_limits: ModelLimits,
    limits_by_model: BTreeMap<String, ModelLimits>,
}

impl ScriptedModelGateway {
    pub fn new(steps: impl IntoIterator<Item = ScriptedStep>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            completion_steps: Mutex::new(VecDeque::new()),
            summary_steps: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            completion_requests: Mutex::new(Vec::new()),
            summary_requests: Mutex::new(Vec::new()),
            default_limits: ModelLimits {
                context_window_tokens: u64::MAX,
                max_output_tokens: u32::MAX,
                max_input_tokens: u64::MAX,
                compact_trigger_tokens: u64::MAX,
            },
            limits_by_model: BTreeMap::new(),
        })
    }

    pub fn with_completions(
        steps: impl IntoIterator<Item = ScriptedStep>,
        completion_steps: impl IntoIterator<Item = ScriptedCompletionStep>,
    ) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            completion_steps: Mutex::new(completion_steps.into_iter().collect()),
            summary_steps: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            completion_requests: Mutex::new(Vec::new()),
            summary_requests: Mutex::new(Vec::new()),
            default_limits: ModelLimits {
                context_window_tokens: u64::MAX,
                max_output_tokens: u32::MAX,
                max_input_tokens: u64::MAX,
                compact_trigger_tokens: u64::MAX,
            },
            limits_by_model: BTreeMap::new(),
        })
    }

    pub fn with_compaction(
        steps: impl IntoIterator<Item = ScriptedStep>,
        summary_steps: impl IntoIterator<Item = ScriptedSummaryStep>,
        limits: ModelLimits,
    ) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            completion_steps: Mutex::new(VecDeque::new()),
            summary_steps: Mutex::new(summary_steps.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            completion_requests: Mutex::new(Vec::new()),
            summary_requests: Mutex::new(Vec::new()),
            default_limits: limits,
            limits_by_model: BTreeMap::new(),
        })
    }

    pub fn with_model_limits(
        steps: impl IntoIterator<Item = ScriptedStep>,
        summary_steps: impl IntoIterator<Item = ScriptedSummaryStep>,
        limits: impl IntoIterator<Item = (String, ModelLimits)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            completion_steps: Mutex::new(VecDeque::new()),
            summary_steps: Mutex::new(summary_steps.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            completion_requests: Mutex::new(Vec::new()),
            summary_requests: Mutex::new(Vec::new()),
            default_limits: ModelLimits {
                context_window_tokens: u64::MAX,
                max_output_tokens: u32::MAX,
                max_input_tokens: u64::MAX,
                compact_trigger_tokens: u64::MAX,
            },
            limits_by_model: limits.into_iter().collect(),
        })
    }

    pub async fn requests(&self) -> Vec<RecordedTurnRequest> {
        self.requests.lock().await.clone()
    }

    pub async fn completion_requests(&self) -> Vec<RecordedCompletionRequest> {
        self.completion_requests.lock().await.clone()
    }

    pub async fn summary_request_count(&self) -> usize {
        self.summary_requests.lock().await.len()
    }

    pub async fn summary_requests(&self) -> Vec<RecordedSummaryRequest> {
        self.summary_requests.lock().await.clone()
    }
}

#[async_trait]
impl ModelClient for ScriptedModelGateway {
    fn model_limits(&self, model: &str) -> Result<ModelLimits, ModelClientError> {
        Ok(self
            .limits_by_model
            .get(model)
            .copied()
            .unwrap_or(self.default_limits))
    }

    async fn stream_turn(
        &self,
        selection: ModelSelection,
        messages: Vec<ContextMessage>,
        _tools: Vec<Value>,
        events: mpsc::UnboundedSender<ModelStreamEvent>,
        cancellation: CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        self.requests.lock().await.push(RecordedTurnRequest {
            selection,
            messages,
        });
        let step = self.steps.lock().await.pop_front().ok_or_else(|| {
            ModelClientError::Protocol("scripted model has no response left".into())
        })?;
        match step {
            ScriptedStep::Response {
                chunks,
                tool_calls,
                finish_reason,
                delay_ms,
                input_tokens,
                output_tokens,
            } => {
                let mut content = String::new();
                for chunk in chunks {
                    if delay_ms > 0 {
                        tokio::select! {
                            _ = cancellation.cancelled() => return Err(ModelClientError::Cancelled),
                            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                        }
                    } else if cancellation.is_cancelled() {
                        return Err(ModelClientError::Cancelled);
                    }
                    content.push_str(&chunk);
                    let _ = events.send(ModelStreamEvent::TextDelta(chunk));
                }
                Ok(ModelReply {
                    content,
                    reasoning: None,
                    tool_calls,
                    finish_reason,
                    usage: ModelUsage {
                        input_tokens,
                        output_tokens,
                        total_tokens: input_tokens.saturating_add(output_tokens),
                    },
                })
            }
            ScriptedStep::ReasoningResponse { reasoning, content } => {
                let _ = events.send(ModelStreamEvent::ReasoningDelta(reasoning.clone()));
                let _ = events.send(ModelStreamEvent::TextDelta(content.clone()));
                Ok(ModelReply {
                    content,
                    reasoning: Some(reasoning),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: ModelUsage::default(),
                })
            }
            ScriptedStep::ContextLengthExceeded => Err(ModelClientError::ContextLengthExceeded {
                status: 400,
                body: "maximum context length exceeded".to_string(),
            }),
        }
    }

    async fn complete(
        &self,
        selection: ModelSelection,
        messages: Vec<ContextMessage>,
        cancellation: CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        if cancellation.is_cancelled() {
            return Err(ModelClientError::Cancelled);
        }
        self.completion_requests
            .lock()
            .await
            .push(RecordedCompletionRequest {
                selection,
                messages,
            });
        let step = self
            .completion_steps
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| {
                ModelClientError::Protocol(
                    "scripted completion response is not configured".to_string(),
                )
            })?;
        Ok(ModelReply {
            content: step.content,
            reasoning: None,
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: ModelUsage {
                input_tokens: step.input_tokens,
                output_tokens: step.output_tokens,
                total_tokens: step.input_tokens.saturating_add(step.output_tokens),
            },
        })
    }

    async fn summarize(
        &self,
        selection: ModelSelection,
        view: CompactionView,
        _cancellation: CancellationToken,
    ) -> Result<SummaryReply, ModelClientError> {
        self.summary_requests
            .lock()
            .await
            .push(RecordedSummaryRequest { selection, view });
        let step = self.summary_steps.lock().await.pop_front().ok_or_else(|| {
            ModelClientError::Protocol("scripted summary response is not configured".to_string())
        })?;
        Ok(SummaryReply {
            summary: step.summary,
            usage: ModelUsage {
                input_tokens: step.input_tokens,
                output_tokens: step.output_tokens,
                total_tokens: step.input_tokens.saturating_add(step.output_tokens),
            },
        })
    }
}
