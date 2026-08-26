use std::sync::Arc;
use std::time::Instant;

use dwo_context::{
    ContextManager, MessageKind, PendingContextMessage, PendingMessageBatch, SessionContext,
    SystemPromptBuilder, TurnId,
};
use dwo_model_client::{
    ModelClient, ModelClientError, ModelReply, ModelSelection, ModelStreamEvent, error_kind,
    retry_info, wait_before_retry,
};
use dwo_tools::{ExecutionContext, ParsedToolCall, ToolEvent, ToolManager, ToolResult};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::compaction::{self, CompactionRequest};
use crate::events::{ActiveToolCall, CompactionTrigger, NotificationLevel};
use crate::permission::PermissionRequester;
use crate::session::ActorEvent;
use crate::session_record::{SessionConfig, SessionId};

const HANDOFF_CONTINUATION: &str = "<handoff_continuation>The handoff has already been completed and the model context has been rebuilt from the handoff summary. Continue the user's original task from this context. Do not call handoff again unless a genuinely new context rebuild is necessary.</handoff_continuation>";

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

pub(crate) struct TurnExecution {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub context: ContextManager,
    pub prompt_builder: SystemPromptBuilder,
    pub model: Arc<dyn ModelClient>,
    pub tools: Arc<ToolManager>,
    pub config: watch::Receiver<SessionConfig>,
    pub max_model_steps: usize,
    pub permission: PermissionRequester,
    pub cancellation: CancellationToken,
    pub steer: mpsc::UnboundedReceiver<PendingContextMessage>,
    pub actor: mpsc::UnboundedSender<ActorEvent>,
}

struct ModelStep {
    selection: ModelSelection,
    provider: String,
    allow_image_input: bool,
}

impl TurnExecution {
    fn emit(&self, event: TurnUpdate) {
        let _ = self.actor.send(ActorEvent::Turn(event));
    }

    async fn checkpoint(&mut self) -> anyhow::Result<()> {
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
}

pub(crate) async fn run(mut turn: TurnExecution) {
    let started = Instant::now();
    let selection = {
        let config = turn.config.borrow();
        ModelSelection {
            model: config.model.clone(),
            reasoning: config.reasoning.clone(),
        }
    };
    tracing::info!(
        event = "turn.started",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %selection.model,
        reasoning = selection.reasoning.as_deref(),
        "turn started"
    );
    let outcome = run_inner(&mut turn).await;
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

async fn run_inner(turn: &mut TurnExecution) -> TurnOutcome {
    let max_model_steps = turn.max_model_steps;
    let mut step = 0usize;
    loop {
        if max_model_steps > 0 && step >= max_model_steps {
            return TurnOutcome::Failed(format!(
                "agent loop exceeded {max_model_steps} model steps"
            ));
        }
        step += 1;
        if turn.cancellation.is_cancelled() {
            return TurnOutcome::Cancelled;
        }

        let builder = turn.prompt_builder.clone();
        let current = match tokio::task::spawn_blocking(move || builder.scan_dynamic()).await {
            Ok(Ok(current)) => current,
            Ok(Err(error)) => {
                return TurnOutcome::Failed(format!("refresh environment: {error:#}"));
            }
            Err(error) => {
                return TurnOutcome::Failed(format!("refresh environment task: {error:#}"));
            }
        };
        turn.context.refresh_environment_snapshot(current);
        let selection = {
            let config = turn.config.borrow();
            ModelSelection {
                model: config.model.clone(),
                reasoning: config.reasoning.clone(),
            }
        };
        let model_step = match (|| {
            Ok::<_, ModelClientError>(ModelStep {
                provider: turn.model.context_owner_id(&selection.model)?,
                allow_image_input: turn.model.supports_image_input(&selection.model)?,
                selection,
            })
        })() {
            Ok(step) => step,
            Err(error) => return TurnOutcome::Failed(format!("resolve model: {error:#}")),
        };
        let previous_provider = if turn.context.context().provider.is_none() {
            match turn
                .context
                .context()
                .usage
                .last_model
                .as_deref()
                .map(|model| turn.model.context_owner_id(model))
                .transpose()
            {
                Ok(provider) => provider,
                Err(error) => {
                    return TurnOutcome::Failed(format!("normalize model context: {error:#}"));
                }
            }
        } else {
            None
        };
        if turn.context.normalize_for_selection(
            &model_step.provider,
            previous_provider.as_deref(),
            model_step.allow_image_input,
        ) && let Err(error) = turn.checkpoint().await
        {
            return TurnOutcome::Failed(format!("normalize model context: {error:#}"));
        }
        let compact_trigger_tokens = match turn.model.model_limits(&model_step.selection.model) {
            Ok(limits) => limits.compact_trigger_tokens,
            Err(error) => return TurnOutcome::Failed(format!("compact context: {error:#}")),
        };
        if turn
            .context
            .compaction_due(compact_trigger_tokens, turn.tools.schemas())
            && let Err(error) = compact(
                turn,
                model_step.selection.clone(),
                None,
                CompactionTrigger::Automatic,
            )
            .await
        {
            return if turn.cancellation.is_cancelled() {
                TurnOutcome::Cancelled
            } else {
                TurnOutcome::Failed(format!("compact context: {error:#}"))
            };
        }

        let response = match request_with_context_recovery(turn, &model_step.selection).await {
            Ok(response) => response,
            Err(error) => {
                if turn.cancellation.is_cancelled() {
                    return TurnOutcome::Cancelled;
                }
                return TurnOutcome::Failed(format!("{error:#}"));
            }
        };

        let active_tool_calls = response
            .tool_calls
            .iter()
            .map(active_tool_call)
            .collect::<Vec<_>>();
        let remote_tool_calls = response
            .remote_tool_calls
            .iter()
            .map(active_tool_call)
            .collect::<Vec<_>>();
        turn.context
            .append_response_items(model_step.provider.clone(), response.context_output_items());
        turn.context
            .record_model_success(model_step.selection.model.clone());
        let reasoning = response.transcript_reasoning();
        turn.emit(TurnUpdate::AssistantCompleted {
            turn_id: turn.turn_id.clone(),
            content: response.content,
            reasoning,
            tool_calls: active_tool_calls
                .iter()
                .filter(|call| call.tool_name != "plan")
                .chain(remote_tool_calls.iter())
                .cloned()
                .collect(),
        });
        for call in &remote_tool_calls {
            let mut started = call.clone();
            started.status = "in_progress".to_string();
            turn.emit(TurnUpdate::ToolChanged {
                turn_id: turn.turn_id.clone(),
                call: started,
            });
            let raw_output = response
                .remote_tool_calls
                .iter()
                .find(|raw| raw.get("id").and_then(Value::as_str) == Some(&call.tool_call_id))
                .and_then(|raw| raw.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            turn.emit(TurnUpdate::ToolCompleted {
                turn_id: turn.turn_id.clone(),
                result: ToolResult {
                    tool_call_id: call.tool_call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    output: raw_output,
                    model_context: Vec::new(),
                },
            });
        }
        if response.tool_calls.is_empty() {
            let pending = turn.take_steer_messages();
            if turn.context.append_pending(pending) {
                if let Err(error) = turn.checkpoint().await {
                    return TurnOutcome::Failed(format!(
                        "persist pending message checkpoint: {error:#}"
                    ));
                }
                continue;
            }
            if let Err(error) = turn.checkpoint().await {
                return TurnOutcome::Failed(format!("persist assistant checkpoint: {error:#}"));
            }
            return TurnOutcome::Completed;
        }

        for mut call in active_tool_calls
            .into_iter()
            .filter(|call| call.tool_name != "plan")
        {
            call.status = "in_progress".to_string();
            turn.emit(TurnUpdate::ToolChanged {
                turn_id: turn.turn_id.clone(),
                call,
            });
        }

        let mut execution = ExecutionContext::new(turn.config.borrow().mode);
        execution.confirmation = Some(turn.permission.confirmation_handler());
        execution.allow_image_input = model_step.allow_image_input;
        let plan_actor = turn.actor.clone();
        let plan_turn_id = turn.turn_id.clone();
        execution.plan = Some(Arc::new(move |request| {
            let actor = plan_actor.clone();
            let turn_id = plan_turn_id.clone();
            Box::pin(async move {
                let (completed, wait) = oneshot::channel();
                actor
                    .send(ActorEvent::Plan {
                        turn_id,
                        request,
                        completed,
                    })
                    .map_err(|_| "session actor stopped".to_string())?;
                wait.await
                    .map_err(|_| "session actor dropped plan request".to_string())?
            })
        }));
        let actor = turn.actor.clone();
        let telemetry_turn_id = turn.turn_id.clone();
        execution.events = Some(Arc::new(move |event| {
            let _ = actor.send(ActorEvent::Turn(TurnUpdate::ToolTelemetry {
                turn_id: telemetry_turn_id.clone(),
                event,
            }));
        }));
        let calls = response.tool_calls;
        let tools_started = Instant::now();
        tracing::info!(
            event = "tool.batch_started",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            tool_count = calls.len(),
            "tool batch started"
        );
        let tool_results = tokio::select! {
            _ = turn.cancellation.cancelled() => cancelled_results(&calls),
            results = turn.tools.execute_batch(calls.clone(), &execution) => results,
        };

        let handoff_ids = tool_results
            .iter()
            .filter(|result| handoff_text(result).is_some())
            .map(|result| result.tool_call_id.clone())
            .collect::<Vec<_>>();
        let context_results = tool_results
            .iter()
            .filter(|result| handoff_text(result).is_none())
            .map(ToolResult::context_record)
            .collect::<Vec<_>>();
        for result in &tool_results {
            tracing::info!(
                event = "tool.call_completed",
                session_id = %turn.session_id,
                turn_id = %turn.turn_id,
                tool_call_id = %result.tool_call_id,
                tool = %result.tool_name,
                status = result.output.get("status").and_then(|value| value.as_str()),
                "tool call completed"
            );
            if result.tool_name != "plan" {
                turn.emit(TurnUpdate::ToolCompleted {
                    turn_id: turn.turn_id.clone(),
                    result: result.clone(),
                });
            }
        }
        tracing::info!(
            event = "tool.batch_completed",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            tool_count = tool_results.len(),
            duration_ms = tools_started.elapsed().as_millis() as u64,
            "tool batch completed"
        );
        turn.context.append_tool_batch(context_results);
        let pending = turn.take_steer_messages();
        turn.context.append_pending(pending);
        if let Some(handoff_text) = tool_results.iter().find_map(handoff_text) {
            for tool_call_id in handoff_ids {
                turn.context.remove_tool_call(&tool_call_id);
            }
            if let Err(error) = compact(
                turn,
                model_step.selection.clone(),
                Some(handoff_text.to_string()),
                CompactionTrigger::Handoff,
            )
            .await
            {
                return TurnOutcome::Failed(format!("apply handoff context: {error:#}"));
            }
            turn.context
                .append_internal(MessageKind::Runtime, HANDOFF_CONTINUATION);
            if let Err(error) = turn.checkpoint().await {
                return TurnOutcome::Failed(format!("persist handoff continuation: {error:#}"));
            }
            tracing::info!(
                event = "context.handoff_continuing",
                session_id = %turn.session_id,
                turn_id = %turn.turn_id,
                "handoff context rebuilt; continuing turn"
            );
            continue;
        }
        if let Err(error) = turn.checkpoint().await {
            return TurnOutcome::Failed(format!("persist tool checkpoint: {error:#}"));
        }
        if turn.cancellation.is_cancelled() {
            return TurnOutcome::Cancelled;
        }
    }
}

async fn request_with_context_recovery(
    turn: &mut TurnExecution,
    selection: &ModelSelection,
) -> anyhow::Result<ModelReply> {
    match request_model(turn, selection).await {
        Ok(response) => Ok(response),
        Err(error)
            if error
                .downcast_ref::<ModelClientError>()
                .is_some_and(ModelClientError::is_context_length_exceeded) =>
        {
            if !compact(turn, selection.clone(), None, CompactionTrigger::Recovery).await? {
                return Err(error);
            }
            request_model(turn, selection).await
        }
        Err(error) => Err(error),
    }
}

async fn request_model(
    turn: &mut TurnExecution,
    selection: &ModelSelection,
) -> anyhow::Result<ModelReply> {
    let mut retry = 0;
    loop {
        let started = Instant::now();
        let messages = turn.context.model_messages().to_vec();
        let message_count = messages.len();
        let tool_count = turn.tools.schemas().len();
        tracing::debug!(
            event = "model.request_started",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            model = %selection.model,
            reasoning = selection.reasoning.as_deref(),
            message_count,
            tool_count,
            attempt = retry + 1,
            "model request started"
        );
        let (response, partial_content, partial_reasoning) = {
            let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();
            let actor = turn.actor.clone();
            let turn_id = turn.turn_id.clone();
            let cancellation = turn.cancellation.clone();
            let model_call = turn.model.stream_turn(
                selection.clone(),
                &messages,
                turn.tools.schemas(),
                chunk_tx,
                &cancellation,
            );
            tokio::pin!(model_call);
            let mut partial_content = String::new();
            let mut partial_reasoning = String::new();
            let response = loop {
                tokio::select! {
                    biased;
                    response = &mut model_call => {
                        while let Ok(event) = chunk_rx.try_recv() {
                            emit_model_event(
                                &actor,
                                &turn_id,
                                event,
                                &mut partial_content,
                                &mut partial_reasoning,
                            );
                        }
                        break response;
                    }
                    Some(event) = chunk_rx.recv() => {
                        emit_model_event(
                            &actor,
                            &turn_id,
                            event,
                            &mut partial_content,
                            &mut partial_reasoning,
                        );
                    }
                }
            };
            (response, partial_content, partial_reasoning)
        };
        match response {
            Ok(reply) => {
                tracing::info!(
                    event = "model.request_completed",
                    session_id = %turn.session_id,
                    turn_id = %turn.turn_id,
                    model = %selection.model,
                    duration_ms = started.elapsed().as_millis() as u64,
                    input_tokens = reply.usage.input_tokens,
                    output_tokens = reply.usage.output_tokens,
                    total_tokens = reply.usage.total_tokens,
                    tool_call_count = reply.tool_calls.len(),
                    finish_reason = ?reply.finish_reason,
                    attempt = retry + 1,
                    "model request completed"
                );
                return Ok(reply);
            }
            Err(error) => {
                tracing::warn!(
                    event = "model.request_failed",
                    session_id = %turn.session_id,
                    turn_id = %turn.turn_id,
                    model = %selection.model,
                    duration_ms = started.elapsed().as_millis() as u64,
                    attempt = retry + 1,
                    error = %error,
                    "model request failed"
                );
                let status = if matches!(error, ModelClientError::Cancelled) {
                    "cancelled"
                } else {
                    "failed"
                };
                turn.emit(TurnUpdate::ToolCallsInterrupted {
                    turn_id: turn.turn_id.clone(),
                    status,
                });
                retry += 1;
                let Some(info) = retry_info(&error, retry) else {
                    if retry > 1 {
                        turn.emit(TurnUpdate::Notification {
                            turn_id: turn.turn_id.clone(),
                            category: "model_retry_exhausted".to_string(),
                            level: NotificationLevel::Error,
                            text: format!(
                                "Model request failed after {} attempts: {}.",
                                retry,
                                human_error_kind(&error)
                            ),
                            data: json!({
                                "attempts": retry,
                                "maxRetries": dwo_model_client::MAX_MODEL_RETRIES,
                                "errorKind": error_kind(&error),
                            }),
                        });
                    }
                    return Err(error.into());
                };
                turn.emit(TurnUpdate::AssistantInterrupted {
                    turn_id: turn.turn_id.clone(),
                    content: partial_content.clone(),
                    reasoning: partial_reasoning,
                    error_kind: info.error_kind.to_string(),
                });
                if !partial_content.is_empty() {
                    turn.context.append_assistant(partial_content, Vec::new());
                }
                let pending = turn.take_steer_messages();
                turn.context.append_pending(pending);
                turn.checkpoint().await?;
                turn.emit(TurnUpdate::Notification {
                    turn_id: turn.turn_id.clone(),
                    category: "model_retrying".to_string(),
                    level: NotificationLevel::Warning,
                    text: format!(
                        "Model request interrupted. Retrying in {:.1}s ({}/{}).",
                        info.delay.as_secs_f64(),
                        info.retry,
                        info.max_retries
                    ),
                    data: json!({
                        "retry": info.retry,
                        "maxRetries": info.max_retries,
                        "delayMs": info.delay.as_millis() as u64,
                        "errorKind": info.error_kind,
                    }),
                });
                wait_before_retry(&info, &turn.cancellation).await?;
                let pending = turn.take_steer_messages();
                if !pending.messages.is_empty() {
                    turn.context.append_pending(pending);
                    turn.checkpoint().await?;
                }
            }
        }
    }
}

fn emit_model_event(
    actor: &mpsc::UnboundedSender<ActorEvent>,
    turn_id: &TurnId,
    event: ModelStreamEvent,
    partial_content: &mut String,
    partial_reasoning: &mut String,
) {
    let emit = |event| {
        let _ = actor.send(ActorEvent::Turn(event));
    };
    match event {
        ModelStreamEvent::TextDelta(delta) => {
            partial_content.push_str(&delta);
            emit(TurnUpdate::AssistantDelta {
                turn_id: turn_id.clone(),
                delta,
            });
        }
        ModelStreamEvent::ReasoningDelta(delta) => {
            partial_reasoning.push_str(&delta);
            emit(TurnUpdate::AssistantReasoningDelta {
                turn_id: turn_id.clone(),
                delta,
            });
        }
        ModelStreamEvent::ToolCall(call) => {
            let call = ActiveToolCall {
                tool_call_id: call.tool_call_id,
                tool_name: call.tool_name,
                raw_input: call.raw_input,
                status: call.status,
            };
            emit(TurnUpdate::ToolChanged {
                turn_id: turn_id.clone(),
                call,
            });
        }
    }
}

fn human_error_kind(error: &ModelClientError) -> &'static str {
    match error_kind(error) {
        "stream_interrupted" => "connection interrupted",
        "rate_limited" => "rate limited",
        "provider_status" => "provider unavailable",
        "http" => "network request failed",
        "protocol" => "provider response was invalid",
        "invalid_response" => "provider response was invalid",
        _ => "request failed",
    }
}

async fn compact(
    turn: &mut TurnExecution,
    selection: ModelSelection,
    supplied_summary: Option<String>,
    trigger: CompactionTrigger,
) -> anyhow::Result<bool> {
    let compaction_id = format!("cmp_{}", uuid::Uuid::new_v4().simple());
    turn.emit(TurnUpdate::Notification {
        turn_id: turn.turn_id.clone(),
        category: "compaction_started".to_string(),
        level: NotificationLevel::Info,
        text: "Compacting context...".to_string(),
        data: json!({
            "compactionId": compaction_id,
            "trigger": trigger,
        }),
    });
    let started = Instant::now();
    tracing::info!(
        event = "context.compaction_started",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %selection.model,
        "context compaction started"
    );
    let result = async {
        let context = std::mem::replace(
            &mut turn.context,
            ContextManager::new(SessionContext::default()),
        );
        let result = compaction::execute(
            context,
            &turn.prompt_builder,
            &turn.model,
            turn.tools.schemas(),
            &turn.cancellation,
            CompactionRequest {
                selection: selection.clone(),
                trigger,
                supplied_summary,
            },
        )
        .await?;
        let compacted = result.compacted;
        turn.context = result.context;
        turn.checkpoint().await?;
        anyhow::Ok((compacted, result.summary))
    }
    .await;
    let (compacted, summary) = match result {
        Ok(result) => result,
        Err(error) => {
            if turn.cancellation.is_cancelled() {
                turn.emit(TurnUpdate::Notification {
                    turn_id: turn.turn_id.clone(),
                    category: "compaction_cancelled".to_string(),
                    level: NotificationLevel::Warning,
                    text: "Context compaction cancelled.".to_string(),
                    data: json!({"compactionId": compaction_id}),
                });
            } else {
                turn.emit(TurnUpdate::Notification {
                    turn_id: turn.turn_id.clone(),
                    category: "compaction_failed".to_string(),
                    level: NotificationLevel::Error,
                    text: "Context compaction failed.".to_string(),
                    data: json!({
                        "compactionId": compaction_id,
                        "error": format!("{error:#}"),
                    }),
                });
            }
            return Err(error);
        }
    };
    turn.emit(TurnUpdate::Notification {
        turn_id: turn.turn_id.clone(),
        category: "compaction_completed".to_string(),
        level: NotificationLevel::Success,
        text: "Context compacted.".to_string(),
        data: json!({
            "compactionId": compaction_id,
            "summary": summary,
            "compacted": compacted,
        }),
    });
    tracing::info!(
        event = "context.compaction_completed",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %selection.model,
        duration_ms = started.elapsed().as_millis() as u64,
        "context compaction completed"
    );
    Ok(compacted)
}

fn handoff_text(result: &ToolResult) -> Option<&str> {
    (result.tool_name == "handoff"
        && result.output.get("status").and_then(Value::as_str) == Some("completed"))
    .then(|| result.output.get("handoff_text").and_then(Value::as_str))
    .flatten()
}

fn cancelled_results(calls: &[Value]) -> Vec<ToolResult> {
    calls
        .iter()
        .map(|call| {
            let (tool_call_id, tool_name) = call_identity(call);
            ToolResult {
                tool_call_id,
                tool_name: tool_name.clone(),
                output: json!({
                    "tool": tool_name,
                    "kind": "other",
                    "status": "cancelled",
                    "error": "turn cancelled",
                }),
                model_context: Vec::new(),
            }
        })
        .collect()
}

fn call_identity(call: &Value) -> (String, String) {
    match ParsedToolCall::parse(call.clone()) {
        Ok(call) => (call.id, call.call.name().to_string()),
        Err(error) => (error.id, error.name),
    }
}

fn active_tool_call(raw: &Value) -> ActiveToolCall {
    match ParsedToolCall::parse(raw.clone()) {
        Ok(call) => ActiveToolCall {
            tool_call_id: call.id,
            tool_name: call.call.name().to_string(),
            raw_input: Value::Object(call.raw_arguments),
            status: "pending".to_string(),
        },
        Err(error) => ActiveToolCall {
            tool_call_id: error.id,
            tool_name: error.name,
            raw_input: raw.get("arguments").cloned().unwrap_or(Value::Null),
            status: raw
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("in_progress")
                .to_string(),
        },
    }
}
