use std::sync::Arc;
use std::time::Instant;

use dwo_context::{
    CompactionPlan, CompactionPlanner, ContextManager, PendingMessageBatch, SessionContext,
    SystemPromptBuilder, TurnId,
};
use dwo_model_client::{ModelClient, ModelReply, ModelSelection, ModelStreamEvent};
use dwo_tools::{ExecutionContext, ParsedToolCall, ToolEvent, ToolManager, ToolResult};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::events::ActiveToolCall;
use crate::permission::PermissionRequester;
use crate::record::{SessionConfig, SessionId, SessionLlmSettings};

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
    ToolStarted {
        turn_id: TurnId,
        call: ActiveToolCall,
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

pub(crate) struct RunTurn {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub context: ContextManager,
    pub prompt_builder: SystemPromptBuilder,
    pub model: Arc<dyn ModelClient>,
    pub tools: Arc<ToolManager>,
    pub config: watch::Receiver<SessionConfig>,
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
    match compact_context(turn, plan, recovery).await {
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
    let max_model_steps = turn.config.borrow().max_model_steps;
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
            Err(_) if turn.cancellation.is_cancelled() => return TurnOutcome::Cancelled,
            Err(error) => return TurnOutcome::Failed(format!("{error:#}")),
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
        turn.emit(TurnEvent::AssistantCompleted {
            turn_id: turn.turn_id.clone(),
            content: response.content,
            reasoning: response.reasoning,
            tool_calls: active_tool_calls
                .iter()
                .chain(remote_tool_calls.iter())
                .cloned()
                .collect(),
        });
        for call in &remote_tool_calls {
            turn.emit(TurnEvent::ToolStarted {
                turn_id: turn.turn_id.clone(),
                call: call.clone(),
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

        for call in active_tool_calls {
            turn.emit(TurnEvent::ToolStarted {
                turn_id: turn.turn_id.clone(),
                call,
            });
        }

        let mut execution = ExecutionContext::new(turn.config.borrow().mode);
        execution.confirmation = Some(turn.permission.confirmation_handler());
        execution.allow_image_input = model_step.allow_image_input;
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

        let context_results = tool_results
            .iter()
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
            turn.emit(TurnEvent::ToolCompleted {
                turn_id: turn.turn_id.clone(),
                result: result.clone(),
            });
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
        Err(error) if error.is_context_length_exceeded() => {
            let recovery = recovery_selection(turn, selection);
            let plan = turn.context.recovery_compaction();
            if !compact_context(turn, plan, recovery).await? {
                return Err(error.into());
            }
            request_model(turn, selection).await.map_err(Into::into)
        }
        Err(error) => Err(error.into()),
    }
}

async fn request_model(
    turn: &mut RunTurn,
    selection: &ModelSelection,
) -> Result<ModelReply, dwo_model_client::ModelClientError> {
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
        "model request started"
    );
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
    loop {
        tokio::select! {
            biased;
            response = &mut model_call => {
                while let Ok(event) = chunk_rx.try_recv() {
                    emit_model_delta(&actor, &turn_id, event);
                }
                match &response {
                    Ok(reply) => tracing::info!(
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
                        "model request completed"
                    ),
                    Err(error) => tracing::warn!(
                        event = "model.request_failed",
                        session_id = %turn.session_id,
                        turn_id = %turn.turn_id,
                        model = %selection.model,
                        duration_ms = started.elapsed().as_millis() as u64,
                        error = %error,
                        "model request failed"
                    ),
                }
                return response;
            }
            Some(event) = chunk_rx.recv() => {
                emit_model_delta(&actor, &turn_id, event);
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
            .map(|model| turn.model.provider_id(model))
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

fn emit_model_delta(
    actor: &mpsc::UnboundedSender<TurnActorMessage>,
    turn_id: &TurnId,
    event: ModelStreamEvent,
) {
    let emit = |event| {
        let _ = actor.send(TurnActorMessage::Event(event));
    };
    match event {
        ModelStreamEvent::TextDelta(delta) => emit(TurnEvent::AssistantDelta {
            turn_id: turn_id.clone(),
            delta,
        }),
        ModelStreamEvent::ReasoningDelta(delta) => emit(TurnEvent::AssistantReasoningDelta {
            turn_id: turn_id.clone(),
            delta,
        }),
    }
}

async fn compact_context(
    turn: &mut RunTurn,
    plan: CompactionPlan,
    selection: ModelSelection,
) -> anyhow::Result<bool> {
    if !plan.needs_replacement() {
        return Ok(false);
    }
    let started = Instant::now();
    tracing::info!(
        event = "context.compaction_started",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %selection.model,
        "context compaction started"
    );
    let model = selection.model.clone();
    let summary = if plan.has_compactable_history() {
        let summary = turn
            .model
            .summarize(selection, plan.view.clone(), turn.cancellation.clone())
            .await?;
        summary.summary
    } else {
        String::new()
    };
    turn.context
        .apply_compaction(plan, summary, &turn.prompt_builder, turn.tools.schemas())?;
    turn.checkpoint().await?;
    tracing::info!(
        event = "context.compaction_completed",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %model,
        duration_ms = started.elapsed().as_millis() as u64,
        "context compaction completed"
    );
    Ok(true)
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
    compact_context(turn, plan, recovery).await?;
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
        provider: turn.model.provider_id(&selection.model)?,
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
                    "kind": "cancelled",
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
        },
        Err(error) => ActiveToolCall {
            tool_call_id: error.id,
            tool_name: error.name,
            raw_input: raw.get("arguments").cloned().unwrap_or(Value::Null),
        },
    }
}
