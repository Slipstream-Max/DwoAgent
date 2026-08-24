use std::sync::Arc;
use std::time::Instant;

use dwo_context::{
    CompactionPlan, CompactionPlanner, ContextManager, MessageKind, PendingMessageBatch,
    SessionContext, SystemPromptBuilder, TurnId,
};
use dwo_model_client::{
    ModelClient, ModelClientError, ModelReply, ModelSelection, ModelStreamEvent, error_kind,
    request_with_retry, retry_info, wait_before_retry,
};
use dwo_tools::{ExecutionContext, ParsedToolCall, ToolEvent, ToolManager, ToolResult};
use dwo_tools::{PlanRequest, PlanResponse};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::{ActiveToolCall, CompactionTrigger, NotificationLevel};
use crate::permission::PermissionRequester;
use crate::record::{SessionConfig, SessionId, SessionLlmSettings};

const HANDOFF_CONTINUATION: &str = "<handoff_continuation>The handoff has already been completed and the model context has been rebuilt from the handoff summary. Continue the user's original task from this context. Do not call handoff again unless a genuinely new context rebuild is necessary.</handoff_continuation>";

pub(crate) enum TurnActorMessage {
    Event(TurnEvent),
    TitleGenerated {
        original_title: String,
        result: Result<ModelReply, dwo_model_client::ModelClientError>,
    },
    PersistContext {
        context: Box<SessionContext>,
        completed: oneshot::Sender<anyhow::Result<()>>,
    },
    TakePendingMessages {
        completed: oneshot::Sender<PendingMessageBatch>,
    },
    Plan {
        turn_id: TurnId,
        request: PlanRequest,
        completed: oneshot::Sender<Result<PlanResponse, String>>,
    },
}

pub(crate) enum TurnEvent {
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
    CompactionStarted {
        turn_id: TurnId,
        compaction_id: String,
        trigger: CompactionTrigger,
    },
    CompactionCompleted {
        turn_id: TurnId,
        compaction_id: String,
        summary: Option<String>,
    },
    CompactionFailed {
        turn_id: TurnId,
        compaction_id: String,
        error: String,
    },
    CompactionCancelled {
        turn_id: TurnId,
        compaction_id: String,
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

pub(crate) struct RunTurn {
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
    pub actor: mpsc::UnboundedSender<TurnActorMessage>,
}

struct ModelStep {
    selection: ModelSelection,
    provider: String,
    allow_image_input: bool,
}

impl RunTurn {
    fn emit(&self, event: TurnEvent) {
        let _ = self.actor.send(TurnActorMessage::Event(event));
    }

    async fn checkpoint(&mut self) -> anyhow::Result<()> {
        normalize_context_for_current_step(self)?;
        let context = self.context.checkpoint(self.tools.schemas());
        let (completed, wait) = oneshot::channel();
        self.actor
            .send(TurnActorMessage::PersistContext {
                context: Box::new(context),
                completed,
            })
            .map_err(|_| anyhow::anyhow!("session actor stopped"))?;
        wait.await
            .map_err(|_| anyhow::anyhow!("session actor dropped checkpoint"))?
    }

    async fn take_pending_messages(&self) -> anyhow::Result<PendingMessageBatch> {
        let (completed, wait) = oneshot::channel();
        self.actor
            .send(TurnActorMessage::TakePendingMessages { completed })
            .map_err(|_| anyhow::anyhow!("session actor stopped"))?;
        wait.await
            .map_err(|_| anyhow::anyhow!("session actor dropped pending message request"))
    }
}

pub(crate) async fn run(mut turn: RunTurn) {
    let started = Instant::now();
    let selection = current_selection(&turn);
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
    turn.emit(TurnEvent::Finished {
        turn_id: turn.turn_id.clone(),
        outcome,
    });
}

pub(crate) async fn run_manual_compaction(mut turn: RunTurn) {
    let started = Instant::now();
    tracing::info!(
        event = "turn.manual_compaction_started",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        "manual context compaction started"
    );
    let outcome = manual_compaction_inner(&mut turn).await;
    match &outcome {
        TurnOutcome::Completed => tracing::info!(
            event = "turn.manual_compaction_completed",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            duration_ms = started.elapsed().as_millis() as u64,
            "manual context compaction completed"
        ),
        TurnOutcome::Cancelled => tracing::info!(
            event = "turn.manual_compaction_cancelled",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            duration_ms = started.elapsed().as_millis() as u64,
            "manual context compaction cancelled"
        ),
        TurnOutcome::Failed(error) => tracing::error!(
            event = "turn.manual_compaction_failed",
            session_id = %turn.session_id,
            turn_id = %turn.turn_id,
            duration_ms = started.elapsed().as_millis() as u64,
            error = %error,
            "manual context compaction failed"
        ),
    }
    turn.emit(TurnEvent::Finished {
        turn_id: turn.turn_id.clone(),
        outcome,
    });
}

async fn manual_compaction_inner(turn: &mut RunTurn) -> TurnOutcome {
    if turn.cancellation.is_cancelled() {
        return TurnOutcome::Cancelled;
    }
    let before = turn.context.context().usage.current_tokens;
    let step = match current_model_step(turn) {
        Ok(step) => step,
        Err(error) => return TurnOutcome::Failed(format!("resolve model: {error:#}")),
    };
    if let Err(error) = prepare_context_for_step(turn, &step).await {
        return TurnOutcome::Failed(format!("normalize model context: {error:#}"));
    }
    let plan = turn.context.plan_compaction(&CompactionPlanner::default());
    let recovery = recovery_selection(turn, &step.selection);
    match compact_context(turn, plan, recovery, CompactionTrigger::Manual).await {
        Ok(compacted) => {
            let content = if compacted {
                format!(
                    "Context compacted from {before} to {} estimated tokens.",
                    turn.context.context().usage.current_tokens
                )
            } else {
                "Nothing to compact.".to_string()
            };
            turn.emit(TurnEvent::AssistantCompleted {
                turn_id: turn.turn_id.clone(),
                content,
                reasoning: None,
                tool_calls: Vec::new(),
            });
            TurnOutcome::Completed
        }
        Err(_) if turn.cancellation.is_cancelled() => TurnOutcome::Cancelled,
        Err(error) => TurnOutcome::Failed(format!("compact context: {error:#}")),
    }
}

async fn run_inner(turn: &mut RunTurn) -> TurnOutcome {
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
        let model_step = match current_model_step(turn) {
            Ok(step) => step,
            Err(error) => return TurnOutcome::Failed(format!("resolve model: {error:#}")),
        };
        if let Err(error) = prepare_context_for_step(turn, &model_step).await {
            return TurnOutcome::Failed(format!("normalize model context: {error:#}"));
        }
        if let Err(error) = compact_if_needed(turn, &model_step).await {
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
        turn.emit(TurnEvent::AssistantCompleted {
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
            turn.emit(TurnEvent::ToolChanged {
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
            turn.emit(TurnEvent::ToolCompleted {
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
            let pending = match turn.take_pending_messages().await {
                Ok(pending) => pending,
                Err(error) => {
                    return TurnOutcome::Failed(format!("receive pending messages: {error:#}"));
                }
            };
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
            turn.emit(TurnEvent::ToolChanged {
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
                    .send(TurnActorMessage::Plan {
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
            let _ = actor.send(TurnActorMessage::Event(TurnEvent::ToolTelemetry {
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
                turn.emit(TurnEvent::ToolCompleted {
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
        let pending = match turn.take_pending_messages().await {
            Ok(pending) => pending,
            Err(error) => {
                return TurnOutcome::Failed(format!("receive pending messages: {error:#}"));
            }
        };
        turn.context.append_pending(pending);
        if let Some(handoff_text) = tool_results.iter().find_map(handoff_text) {
            for tool_call_id in handoff_ids {
                turn.context.remove_tool_call(&tool_call_id);
            }
            let plan = turn.context.plan_compaction(&CompactionPlanner::default());
            if let Err(error) = compact_context_with_summary(
                turn,
                plan,
                handoff_text.to_string(),
                &model_step.selection,
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
    turn: &mut RunTurn,
    selection: &ModelSelection,
) -> anyhow::Result<ModelReply> {
    match request_model(turn, selection).await {
        Ok(response) => Ok(response),
        Err(error)
            if error
                .downcast_ref::<ModelClientError>()
                .is_some_and(ModelClientError::is_context_length_exceeded) =>
        {
            let recovery = recovery_selection(turn, selection);
            let plan = turn.context.recovery_compaction();
            if !compact_context(turn, plan, recovery, CompactionTrigger::Recovery).await? {
                return Err(error);
            }
            request_model(turn, selection).await
        }
        Err(error) => Err(error),
    }
}

async fn request_model(
    turn: &mut RunTurn,
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
                turn.emit(TurnEvent::ToolCallsInterrupted {
                    turn_id: turn.turn_id.clone(),
                    status,
                });
                retry += 1;
                let Some(info) = retry_info(&error, retry) else {
                    if retry > 1 {
                        turn.emit(TurnEvent::Notification {
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
                turn.emit(TurnEvent::AssistantInterrupted {
                    turn_id: turn.turn_id.clone(),
                    content: partial_content.clone(),
                    reasoning: partial_reasoning,
                    error_kind: info.error_kind.to_string(),
                });
                if !partial_content.is_empty() {
                    turn.context.append_assistant(partial_content, Vec::new());
                }
                let pending = turn.take_pending_messages().await?;
                turn.context.append_pending(pending);
                turn.checkpoint().await?;
                turn.emit(TurnEvent::Notification {
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
                let pending = turn.take_pending_messages().await?;
                if !pending.messages.is_empty() {
                    turn.context.append_pending(pending);
                    turn.checkpoint().await?;
                }
            }
        }
    }
}

fn normalize_context_for_step(turn: &mut RunTurn, step: &ModelStep) -> anyhow::Result<bool> {
    let previous_provider = if turn.context.context().provider.is_none() {
        turn.context
            .context()
            .usage
            .last_model
            .as_deref()
            .map(|model| turn.model.context_owner_id(model))
            .transpose()?
    } else {
        None
    };
    Ok(turn.context.normalize_for_selection(
        &step.provider,
        previous_provider.as_deref(),
        step.allow_image_input,
    ))
}

fn normalize_context_for_current_step(turn: &mut RunTurn) -> anyhow::Result<bool> {
    let step = current_model_step(turn)?;
    normalize_context_for_step(turn, &step)
}

async fn prepare_context_for_step(turn: &mut RunTurn, step: &ModelStep) -> anyhow::Result<()> {
    if normalize_context_for_step(turn, step)? {
        turn.checkpoint().await?;
    }
    Ok(())
}

fn emit_model_event(
    actor: &mpsc::UnboundedSender<TurnActorMessage>,
    turn_id: &TurnId,
    event: ModelStreamEvent,
    partial_content: &mut String,
    partial_reasoning: &mut String,
) {
    let emit = |event| {
        let _ = actor.send(TurnActorMessage::Event(event));
    };
    match event {
        ModelStreamEvent::TextDelta(delta) => {
            partial_content.push_str(&delta);
            emit(TurnEvent::AssistantDelta {
                turn_id: turn_id.clone(),
                delta,
            });
        }
        ModelStreamEvent::ReasoningDelta(delta) => {
            partial_reasoning.push_str(&delta);
            emit(TurnEvent::AssistantReasoningDelta {
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
            emit(TurnEvent::ToolChanged {
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

async fn compact_context(
    turn: &mut RunTurn,
    plan: CompactionPlan,
    selection: ModelSelection,
    trigger: CompactionTrigger,
) -> anyhow::Result<bool> {
    if !plan.needs_replacement() {
        return Ok(false);
    }
    perform_compaction(turn, plan, selection, None, trigger).await?;
    Ok(true)
}

async fn compact_context_with_summary(
    turn: &mut RunTurn,
    plan: CompactionPlan,
    summary: String,
    selection: &ModelSelection,
    trigger: CompactionTrigger,
) -> anyhow::Result<()> {
    perform_compaction(turn, plan, selection.clone(), Some(summary), trigger).await
}

async fn perform_compaction(
    turn: &mut RunTurn,
    plan: CompactionPlan,
    selection: ModelSelection,
    supplied_summary: Option<String>,
    trigger: CompactionTrigger,
) -> anyhow::Result<()> {
    let compaction_id = format!("cmp_{}", Uuid::new_v4().simple());
    turn.emit(TurnEvent::CompactionStarted {
        turn_id: turn.turn_id.clone(),
        compaction_id: compaction_id.clone(),
        trigger,
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
        let summary = match supplied_summary {
            Some(summary) => summary,
            None if plan.has_compactable_history() => {
                request_with_retry(&turn.cancellation, || {
                    turn.model.summarize(
                        selection.clone(),
                        plan.view.clone(),
                        turn.cancellation.clone(),
                    )
                })
                .await?
                .summary
            }
            None => String::new(),
        };
        let display_summary = (!summary.is_empty()).then(|| summary.clone());
        turn.context
            .apply_compaction(plan, summary, &turn.prompt_builder, turn.tools.schemas())?;
        turn.checkpoint().await?;
        anyhow::Ok(display_summary)
    }
    .await;
    let display_summary = match result {
        Ok(summary) => summary,
        Err(error) => {
            if turn.cancellation.is_cancelled() {
                turn.emit(TurnEvent::CompactionCancelled {
                    turn_id: turn.turn_id.clone(),
                    compaction_id,
                });
            } else {
                turn.emit(TurnEvent::CompactionFailed {
                    turn_id: turn.turn_id.clone(),
                    compaction_id,
                    error: format!("{error:#}"),
                });
            }
            return Err(error);
        }
    };
    turn.emit(TurnEvent::CompactionCompleted {
        turn_id: turn.turn_id.clone(),
        compaction_id,
        summary: display_summary,
    });
    tracing::info!(
        event = "context.compaction_completed",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %selection.model,
        duration_ms = started.elapsed().as_millis() as u64,
        "context compaction completed"
    );
    Ok(())
}

fn handoff_text(result: &ToolResult) -> Option<&str> {
    (result.tool_name == "handoff"
        && result.output.get("status").and_then(Value::as_str) == Some("completed"))
    .then(|| result.output.get("handoff_text").and_then(Value::as_str))
    .flatten()
}

async fn compact_if_needed(turn: &mut RunTurn, step: &ModelStep) -> anyhow::Result<()> {
    let trigger_tokens = turn
        .model
        .model_limits(&step.selection.model)?
        .compact_trigger_tokens;
    let schemas = turn.tools.schemas();
    let Some(plan) = turn.context.scheduled_compaction(trigger_tokens, schemas) else {
        return Ok(());
    };
    let recovery = recovery_selection(turn, &step.selection);
    compact_context(turn, plan, recovery, CompactionTrigger::Automatic).await?;
    Ok(())
}

fn current_selection(turn: &RunTurn) -> ModelSelection {
    let settings = llm_settings(&turn.config.borrow());
    ModelSelection {
        model: settings.model,
        reasoning: settings.reasoning,
    }
}

fn current_model_step(turn: &RunTurn) -> Result<ModelStep, dwo_model_client::ModelClientError> {
    let selection = current_selection(turn);
    Ok(ModelStep {
        provider: turn.model.context_owner_id(&selection.model)?,
        allow_image_input: turn.model.supports_image_input(&selection.model)?,
        selection,
    })
}

fn recovery_selection(turn: &RunTurn, desired: &ModelSelection) -> ModelSelection {
    let usage = &turn.context.context().usage;
    ModelSelection {
        model: usage
            .last_model
            .clone()
            .unwrap_or_else(|| desired.model.clone()),
        reasoning: desired.reasoning.clone(),
    }
}

fn llm_settings(config: &SessionConfig) -> SessionLlmSettings {
    SessionLlmSettings::new(config.model.clone(), config.reasoning.clone())
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
