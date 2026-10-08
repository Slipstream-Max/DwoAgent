use std::sync::Arc;
use std::time::Instant;

use dwo_context::{
    ContextManager, PendingContextMessage, PendingMessageBatch, SystemPromptBuilder, TurnId,
};
use dwo_model_client::{ModelClient, ModelReply, ModelSelection};
use dwo_tools::{ToolEvent, ToolManager, ToolResult};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::events::{ActiveToolCall, NotificationLevel};
use crate::permission::PermissionRequester;
use crate::session::ActorEvent;
use crate::session_record::{SessionConfig, SessionId};
use crate::tool_calls::ParsedToolCalls;

pub(crate) enum TurnUpdate {
    AssistantDelta {
        turn_id: TurnId,
        delta: String,
    },
    AssistantReasoningDelta {
        turn_id: TurnId,
        delta: String,
    },
    AssistantCompleted {
        turn_id: TurnId,
        content: String,
        reasoning: Option<String>,
        tool_calls: Vec<ActiveToolCall>,
    },
    AssistantInterrupted {
        turn_id: TurnId,
        content: String,
        reasoning: String,
        error_kind: String,
    },
    Notification {
        turn_id: TurnId,
        category: String,
        level: NotificationLevel,
        text: String,
        data: Value,
    },
    ToolChanged {
        turn_id: TurnId,
        call: ActiveToolCall,
    },
    ToolCallsInterrupted {
        turn_id: TurnId,
        status: &'static str,
    },
    ToolCompleted {
        turn_id: TurnId,
        result: ToolResult,
    },
    ToolTelemetry {
        turn_id: TurnId,
        event: ToolEvent,
    },
    Finished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
}

pub(crate) enum TurnOutcome {
    Completed,
    Cancelled,
    Failed(String),
}

pub(crate) enum TurnError {
    Cancelled,
    Failed(String),
}

pub(crate) struct TurnExecution {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub context: ContextManager,
    pub prompt_builder: SystemPromptBuilder,
    pub model: Arc<dyn ModelClient>,
    pub tools: Arc<ToolManager>,
    pub config: watch::Receiver<SessionConfig>,
    pub permission: PermissionRequester,
    pub cancellation: CancellationToken,
    pub steer: mpsc::UnboundedReceiver<PendingContextMessage>,
    pub actor: mpsc::UnboundedSender<ActorEvent>,
}

pub(crate) struct ModelStep {
    pub(crate) selection: ModelSelection,
    pub(crate) provider: String,
    pub(crate) allow_image_input: bool,
}

impl TurnExecution {
    pub(crate) fn emit(&self, event: TurnUpdate) {
        let _ = self.actor.send(ActorEvent::Turn(event));
    }

    pub(crate) async fn checkpoint(&mut self) -> anyhow::Result<()> {
        let context = self.context.checkpoint(self.tools.schemas());
        let (completed, wait) = oneshot::channel();
        self.actor
            .send(ActorEvent::PersistContext {
                context: Box::new(context),
                completed,
            })
            .map_err(|_| anyhow::anyhow!("session actor stopped"))?;
        wait.await
            .map_err(|_| anyhow::anyhow!("session actor dropped checkpoint"))?
    }

    fn take_steer_messages(&mut self) -> PendingMessageBatch {
        let mut messages = Vec::new();
        while let Ok(message) = self.steer.try_recv() {
            messages.push(message);
        }
        PendingMessageBatch {
            should_continue: !messages.is_empty(),
            messages,
        }
    }

    fn current_selection(&self) -> ModelSelection {
        let config = self.config.borrow();
        ModelSelection {
            model: config.model.clone(),
            reasoning: config.reasoning.clone(),
        }
    }

    pub(crate) fn fail(&self, context: &str, error: impl std::fmt::Display) -> TurnError {
        if self.cancellation.is_cancelled() {
            TurnError::Cancelled
        } else {
            TurnError::Failed(format!("{context}: {error}"))
        }
    }

    pub(crate) fn fail_bare(&self, error: impl std::fmt::Display) -> TurnError {
        if self.cancellation.is_cancelled() {
            TurnError::Cancelled
        } else {
            TurnError::Failed(format!("{error}"))
        }
    }

    pub(crate) async fn settle(&mut self) -> anyhow::Result<bool> {
        let pending = self.take_steer_messages();
        let should_continue = self.context.append_pending(pending);
        self.checkpoint().await?;
        Ok(should_continue)
    }
}

pub(crate) async fn run(mut turn: TurnExecution) {
    let started = Instant::now();
    let selection = turn.current_selection();
    tracing::info!(
        event = "turn.started",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %selection.model,
        reasoning = selection.reasoning.as_deref(),
        "turn started"
    );
    let outcome = run_loop(&mut turn).await;
    match &outcome {
        TurnOutcome::Completed => tracing::info!(
            event = "turn.completed",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            duration_ms = started.elapsed().as_millis() as u64,
            "turn completed"
        ),
        TurnOutcome::Cancelled => tracing::info!(
            event = "turn.cancelled",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            duration_ms = started.elapsed().as_millis() as u64,
            "turn cancelled"
        ),
        TurnOutcome::Failed(error) => tracing::error!(
            event = "turn.failed",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            duration_ms = started.elapsed().as_millis() as u64,
            error = %error,
            "turn failed"
        ),
    }
    turn.emit(TurnUpdate::Finished {
        turn_id: turn.turn_id.clone(),
        outcome,
    });
}

async fn run_loop(turn: &mut TurnExecution) -> TurnOutcome {
    let result: Result<(), TurnError> = async {
        loop {
            if turn.cancellation.is_cancelled() {
                return Err(TurnError::Cancelled);
            }

            let model_step = turn.begin_step().await?;
            turn.maybe_compact(&model_step).await?;
            let reply = turn.request(&model_step).await?;
            if !turn.apply_reply(&model_step, reply).await? {
                return Ok(());
            }
        }
    }
    .await;
    match result {
        Ok(()) => TurnOutcome::Completed,
        Err(TurnError::Cancelled) => TurnOutcome::Cancelled,
        Err(TurnError::Failed(message)) => TurnOutcome::Failed(message),
    }
}

impl TurnExecution {
    async fn begin_step(&mut self) -> Result<ModelStep, TurnError> {
        let builder = self.prompt_builder.clone();
        let current = tokio::task::spawn_blocking(move || builder.scan_dynamic())
            .await
            .map_err(|error| self.fail("refresh environment task", format!("{error:#}")))?
            .map_err(|error| self.fail("refresh environment", format!("{error:#}")))?;
        self.context.refresh_environment_snapshot(current);

        let selection = self.current_selection();
        let provider = self
            .model
            .context_owner_id(&selection.model)
            .map_err(|error| self.fail("resolve model", format!("{error:#}")))?;
        let allow_image_input = self
            .model
            .supports_image_input(&selection.model)
            .map_err(|error| self.fail("resolve model", format!("{error:#}")))?;

        let previous_provider = if self.context.context().provider.is_none() {
            self.context
                .context()
                .usage
                .last_model
                .as_deref()
                .map(|model| self.model.context_owner_id(model))
                .transpose()
                .map_err(|error| self.fail("normalize model context", format!("{error:#}")))?
        } else {
            None
        };
        if self.context.normalize_for_selection(
            &provider,
            previous_provider.as_deref(),
            allow_image_input,
        ) {
            let checkpoint = self.checkpoint().await;
            if let Err(error) = checkpoint {
                return Err(self.fail("normalize model context", format!("{error:#}")));
            }
        }

        Ok(ModelStep {
            selection,
            provider,
            allow_image_input,
        })
    }

    async fn apply_reply(
        &mut self,
        model_step: &ModelStep,
        response: ModelReply,
    ) -> Result<bool, TurnError> {
        let parsed = ParsedToolCalls::from_reply(&response);
        self.context
            .append_response_items(model_step.provider.clone(), response.context_output_items());
        self.context
            .record_model_success(model_step.selection.model.clone());
        self.context
            .record_model_usage(response.usage.total_tokens, self.tools.schemas());
        let reasoning = response.transcript_reasoning();
        self.emit(TurnUpdate::AssistantCompleted {
            turn_id: self.turn_id.clone(),
            content: response.content,
            reasoning,
            tool_calls: parsed.assistant_visible(),
        });
        self.replay_remote_tool_calls(&parsed);

        if parsed.local.is_empty() {
            let settled = self.settle().await;
            return settled.map_err(|error| self.fail("persist checkpoint", format!("{error:#}")));
        }

        self.run_tool_batch(model_step, parsed).await?;
        Ok(true)
    }
}
