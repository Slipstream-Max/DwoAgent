use std::sync::Arc;

use dwo_context::{
    CompactionPlan, CompactionPlanner, ContextManager, SessionContext, SystemPromptBuilder,
    ToolResultRecord, TurnId,
};
use dwo_model_client::{ModelClient, ModelReply, ModelSelection, ModelStreamEvent};
use dwo_tools::{ExecutionContext, ParsedToolCall, ToolManager, ToolResult};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::events::ActiveToolCall;
use crate::permission::PermissionRequester;
use crate::record::{SessionConfig, SessionLlmSettings};

const MAX_MODEL_STEPS: usize = 100;

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

impl RunTurn {
    fn emit(&self, event: TurnEvent) {
        let _ = self.actor.send(TurnActorMessage::Event(event));
    }

    async fn checkpoint(&self) -> anyhow::Result<()> {
        let (completed, wait) = oneshot::channel();
        self.actor
            .send(TurnActorMessage::PersistContext {
                context: Box::new(self.context.context().clone()),
                completed,
            })
            .map_err(|_| anyhow::anyhow!("session actor stopped"))?;
        wait.await
            .map_err(|_| anyhow::anyhow!("session actor dropped checkpoint"))?
    }
}

pub(crate) async fn run(mut turn: RunTurn) {
    let outcome = run_inner(&mut turn).await;
    turn.emit(TurnEvent::Finished {
        turn_id: turn.turn_id.clone(),
        outcome,
    });
}

async fn run_inner(turn: &mut RunTurn) -> TurnOutcome {
    for _ in 0..MAX_MODEL_STEPS {
        if turn.cancellation.is_cancelled() {
            return TurnOutcome::Cancelled;
        }

        if let Err(error) = turn.context.refresh_environment(&turn.prompt_builder) {
            return TurnOutcome::Failed(format!("refresh environment: {error:#}"));
        }
        refresh_context_usage(turn);

        let desired = current_selection(turn);
        let limits = match turn.model.model_limits(&desired.model) {
            Ok(limits) => limits,
            Err(error) => return TurnOutcome::Failed(format!("compact context: {error:#}")),
        };
        if turn.context.should_compact(limits.compact_trigger_tokens) {
            let plan = turn.context.plan_compaction(&CompactionPlanner::default());
            let recovery = recovery_selection(turn, &desired);
            if let Err(error) = compact_context(turn, plan, recovery).await {
                return if turn.cancellation.is_cancelled() {
                    TurnOutcome::Cancelled
                } else {
                    TurnOutcome::Failed(format!("compact context: {error:#}"))
                };
            }
        }

        let selection = current_selection(turn);
        let response = match request_with_context_recovery(turn, &selection).await {
            Ok(response) => response,
            Err(_) if turn.cancellation.is_cancelled() => return TurnOutcome::Cancelled,
            Err(error) => return TurnOutcome::Failed(format!("{error:#}")),
        };

        let active_tool_calls = response
            .tool_calls
            .iter()
            .map(active_tool_call)
            .collect::<Vec<_>>();
        turn.context.append_assistant_with_reasoning(
            turn.turn_id.clone(),
            response.content.clone(),
            response.reasoning.clone(),
            response.tool_calls.clone(),
        );
        turn.context.record_model_success(selection.model);
        refresh_context_usage(turn);
        turn.emit(TurnEvent::AssistantCompleted {
            turn_id: turn.turn_id.clone(),
            content: response.content,
            reasoning: response.reasoning,
            tool_calls: active_tool_calls.clone(),
        });
        if response.tool_calls.is_empty() {
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
        let calls = response.tool_calls;
        let tool_results = tokio::select! {
            _ = turn.cancellation.cancelled() => cancelled_results(&calls),
            results = turn.tools.execute_batch(calls.clone(), &execution) => results,
        };

        let context_results = tool_results
            .iter()
            .map(|result| ToolResultRecord {
                tool_call_id: result.tool_call_id.clone(),
                tool_name: result.tool_name.clone(),
                output: result.output.clone(),
            })
            .collect::<Vec<_>>();
        for (result, context_result) in tool_results.into_iter().zip(context_results) {
            turn.emit(TurnEvent::ToolCompleted {
                turn_id: turn.turn_id.clone(),
                result: result.clone(),
            });
            turn.context
                .append_tool(turn.turn_id.clone(), context_result);
        }
        refresh_context_usage(turn);
        if let Err(error) = turn.checkpoint().await {
            return TurnOutcome::Failed(format!("persist tool checkpoint: {error:#}"));
        }
        if turn.cancellation.is_cancelled() {
            return TurnOutcome::Cancelled;
        }
    }

    TurnOutcome::Failed(format!("agent loop exceeded {MAX_MODEL_STEPS} model steps"))
}

async fn request_with_context_recovery(
    turn: &mut RunTurn,
    selection: &ModelSelection,
) -> anyhow::Result<ModelReply> {
    match request_model(turn, selection).await {
        Ok(response) => Ok(response),
        Err(error) if error.is_context_length_exceeded() => {
            let recovery = recovery_selection(turn, selection);
            let plan = turn
                .context
                .plan_recovery_compaction(&CompactionPlanner::default());
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
    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();
    let model_call = turn.model.stream_turn(
        selection.clone(),
        turn.context.model_messages().to_vec(),
        turn.tools.schemas(),
        chunk_tx,
        turn.cancellation.clone(),
    );
    tokio::pin!(model_call);
    loop {
        tokio::select! {
            biased;
            response = &mut model_call => {
                while let Ok(event) = chunk_rx.try_recv() {
                    emit_model_delta(turn, event);
                }
                return response;
            }
            Some(event) = chunk_rx.recv() => {
                emit_model_delta(turn, event);
            }
        }
    }
}

fn emit_model_delta(turn: &RunTurn, event: ModelStreamEvent) {
    match event {
        ModelStreamEvent::TextDelta(delta) => turn.emit(TurnEvent::AssistantDelta {
            turn_id: turn.turn_id.clone(),
            delta,
        }),
        ModelStreamEvent::ReasoningDelta(delta) => turn.emit(TurnEvent::AssistantReasoningDelta {
            turn_id: turn.turn_id.clone(),
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
        .apply_compaction(plan, summary, &turn.prompt_builder, &turn.tools.schemas())?;
    turn.checkpoint().await?;
    Ok(true)
}

fn refresh_context_usage(turn: &mut RunTurn) {
    turn.context.refresh_usage(&turn.tools.schemas());
}

fn current_selection(turn: &RunTurn) -> ModelSelection {
    let settings = llm_settings(&turn.config.borrow());
    ModelSelection {
        model: settings.model,
        reasoning: settings.reasoning,
    }
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
    SessionLlmSettings {
        model: config.model.clone(),
        reasoning: config.reasoning.clone(),
    }
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
