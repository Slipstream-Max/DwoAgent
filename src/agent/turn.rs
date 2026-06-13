//! Turn runner.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Map, Value, json};

use super::activity::ActivityTurnHandle;
use super::constants::{
    PERMISSION_CANCELLED, STOP_CANCELLED, STOP_COMPLETED, STOP_MAX_TURNS, cancelled_tool_output,
};
use super::policy::{ToolPolicyAction, resolve_permission_decision, resolve_tool_policy};
use crate::config::policy::ToolPolicyConfig;
use crate::context::manager::{
    CancelEvent, CompactionOutcome, ConversationContextManager, SystemMessagesBuilder,
};
use crate::llm::client::{BaseLlmClient, LlmRequestCancelled, TOOL_ARG_PARSE_ERROR_FIELD};
use crate::tools::tool_run_manager::ToolRunManager;
use crate::watchers::runtime::WatcherRuntime;

/// Signal raised by the turn runner when the `CancelEvent` fires mid-loop.
#[derive(Debug)]
pub struct TurnCancelled {
    pub tool_outputs: Option<Vec<Value>>,
}

impl TurnCancelled {
    fn new() -> Self {
        Self { tool_outputs: None }
    }

    fn with_outputs(outputs: Vec<Value>) -> Self {
        Self {
            tool_outputs: Some(outputs),
        }
    }
}

impl std::fmt::Display for TurnCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Current turn was cancelled.")
    }
}

impl std::error::Error for TurnCancelled {}

/// Result payload matching Python's `{stop_reason}` dict.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub stop_reason: String,
}

pub type PolicyModeGetter =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = String> + Send>> + Send + Sync>;

/// Bundle of runtime hooks required by [`run_turn`]. Keeping them in one
/// struct avoids passing the same hooks down to every helper.
pub struct TurnRuntime<'a> {
    pub user_input: Value,
    pub get_policy_mode: PolicyModeGetter,
    pub max_running_turn: Option<u32>,
    pub activity: ActivityTurnHandle,
    pub reasoning_mode: String,
    pub model_client: &'a BaseLlmClient,
    pub tool_schemas: Arc<Vec<Value>>,
    pub tool_policy: Arc<ToolPolicyConfig>,
    pub tool_manager: &'a ToolRunManager,
    pub context_manager: &'a mut ConversationContextManager,
    pub rebuild_system_messages: Option<SystemMessagesBuilder>,
    pub watcher_runtime: Option<Arc<WatcherRuntime>>,
}

/// Run a single prompt's multi-turn loop until the model stops, cancellation
/// fires, an unrecoverable error bubbles out, or an optional max-turn guard
/// trips.
pub async fn run_turn(mut runtime: TurnRuntime<'_>) -> Result<TurnResult> {
    inject_pending_watcher_content(&mut runtime).await;
    runtime
        .context_manager
        .add_user(runtime.user_input.clone())?;

    let mut completed_turns = 0_u32;
    while !runtime.activity.cancel_event().is_set() {
        if let Some(max_running_turn) = runtime.max_running_turn
            && completed_turns >= max_running_turn
        {
            return Ok(TurnResult {
                stop_reason: STOP_MAX_TURNS.to_string(),
            });
        }

        let stable_message_count = runtime.context_manager.messages().len();
        let mut parsed_tool_calls: Vec<Value> = Vec::new();

        if let Err(err) = raise_if_cancelled(runtime.activity.cancel_event()) {
            return handle_cancel(
                &mut runtime,
                &mut parsed_tool_calls,
                err,
                stable_message_count,
            )
            .await;
        }

        let turn_step =
            run_turn_step(&mut runtime, &mut parsed_tool_calls, stable_message_count).await;

        match turn_step {
            Ok(TurnStepOutcome::Completed) => {
                return Ok(TurnResult {
                    stop_reason: STOP_COMPLETED.to_string(),
                });
            }
            Ok(TurnStepOutcome::Continue) => {
                completed_turns += 1;
                continue;
            }
            Err(err) => {
                return handle_cancel(
                    &mut runtime,
                    &mut parsed_tool_calls,
                    err,
                    stable_message_count,
                )
                .await;
            }
        }
    }

    Ok(TurnResult {
        stop_reason: STOP_CANCELLED.to_string(),
    })
}

enum TurnStepOutcome {
    Completed,
    Continue,
}

struct AssistantStreamResult {
    message: Map<String, Value>,
    finish_reason: Option<String>,
}

async fn run_turn_step(
    runtime: &mut TurnRuntime<'_>,
    parsed_tool_calls: &mut Vec<Value>,
    _stable_message_count: usize,
) -> Result<TurnStepOutcome> {
    let assistant = request_assistant_message_stream(runtime).await?;
    emit_finish_reason_notice(runtime, assistant.finish_reason.as_deref()).await?;
    let mut calls = runtime.model_client.parse_tool_calls(&assistant.message)?;
    annotate_tool_arg_parse_errors(
        &mut calls,
        assistant.finish_reason.as_deref(),
        runtime.model_client.config.max_tokens,
    );
    *parsed_tool_calls = calls;

    if parsed_tool_calls.is_empty() {
        raise_if_cancelled(runtime.activity.cancel_event())?;
        maybe_compact_with_activity(runtime).await?;
        raise_if_cancelled(runtime.activity.cancel_event())?;
        return Ok(TurnStepOutcome::Completed);
    }

    let tool_outputs = process_tool_calls(runtime, parsed_tool_calls).await?;
    add_tool_results(runtime.context_manager, parsed_tool_calls, &tool_outputs);
    raise_if_cancelled(runtime.activity.cancel_event())?;
    maybe_compact_with_activity(runtime).await?;
    raise_if_cancelled(runtime.activity.cancel_event())?;
    Ok(TurnStepOutcome::Continue)
}

async fn handle_cancel(
    runtime: &mut TurnRuntime<'_>,
    parsed_tool_calls: &mut Vec<Value>,
    err: anyhow::Error,
    stable_message_count: usize,
) -> Result<TurnResult> {
    // Only the sentinel `TurnCancelled` error is swallowed — everything else
    // propagates to the caller after rolling back the partially appended
    // assistant message (matching Python's `except Exception: truncate…`).
    let cancel_payload = err
        .downcast_ref::<TurnCancelled>()
        .map(|c| c.tool_outputs.clone());
    let Some(cancel_outputs) = cancel_payload else {
        runtime
            .context_manager
            .truncate_messages(stable_message_count);
        return Err(err);
    };

    if !parsed_tool_calls.is_empty() {
        let outputs = match cancel_outputs {
            Some(v) if !v.is_empty() => v,
            _ => parsed_tool_calls
                .iter()
                .map(|_| cancelled_tool_output())
                .collect(),
        };
        add_tool_results(runtime.context_manager, parsed_tool_calls, &outputs);
    } else {
        runtime
            .context_manager
            .truncate_messages(stable_message_count);
    }

    Ok(TurnResult {
        stop_reason: STOP_CANCELLED.to_string(),
    })
}

// ── Runtime activity boxes ─────────────────────────────────────────────────

async fn maybe_compact_with_activity(runtime: &mut TurnRuntime<'_>) -> Result<()> {
    if !runtime.context_manager.should_compact() {
        return Ok(());
    }

    let activity_box = runtime.activity.activity_box("Context compaction");
    activity_box
        .start_or_update("in_progress", "Compressing older context...")
        .await?;
    let llm_options = runtime
        .activity
        .llm_request_options(Some(activity_box.retry_callback()));

    let outcome = runtime
        .context_manager
        .maybe_compact(
            runtime.model_client,
            Some(runtime.activity.cancel_event()),
            runtime.rebuild_system_messages.clone(),
            Some(&runtime.reasoning_mode),
            llm_options,
        )
        .await;

    match outcome {
        CompactionOutcome::Compacted => {
            activity_box
                .complete_if_started("Context compacted.")
                .await?;
            runtime
                .activity
                .usage_update(runtime.context_manager.usage_snapshot())
                .await?;
        }
        CompactionOutcome::Skipped => {
            activity_box
                .complete_if_started("Context compaction was not needed.")
                .await?;
        }
        CompactionOutcome::Failed => {
            activity_box
                .fail_if_started("Context compaction failed; continuing with existing context.")
                .await?;
        }
    }

    Ok(())
}

// ── LLM streaming ──────────────────────────────────────────────────────────

async fn request_assistant_message_stream(
    runtime: &mut TurnRuntime<'_>,
) -> Result<AssistantStreamResult> {
    inject_pending_watcher_content(runtime).await;
    let messages_for_model = runtime
        .context_manager
        .messages_for_model(runtime.model_client.capabilities.vision);
    let probes = runtime.activity.assistant_stream_probes();
    let request_tools = if runtime.tool_schemas.is_empty() {
        None
    } else {
        Some(runtime.tool_schemas.as_slice())
    };

    let response_result = runtime
        .model_client
        .request_stream_with_usage(
            messages_for_model.as_ref(),
            None,
            request_tools,
            Some(probes.on_text_chunk),
            Some(probes.on_reasoning_chunk),
            Some(&runtime.reasoning_mode),
            probes.options,
        )
        .await;

    let response = match response_result {
        Ok(response) => response,
        Err(err)
            if runtime.activity.cancel_event().is_set()
                || err.downcast_ref::<LlmRequestCancelled>().is_some() =>
        {
            return Err(anyhow::Error::new(TurnCancelled::new()));
        }
        Err(err) => {
            probes
                .retry_box
                .fail_if_started("Model stream retry failed.")
                .await?;
            return Err(err);
        }
    };

    probes
        .retry_box
        .complete_if_started("Model stream recovered.")
        .await?;

    let total_tokens = response.total_tokens;
    runtime.context_manager.sync_token_usage(total_tokens);
    runtime
        .activity
        .usage_update(runtime.context_manager.usage_snapshot())
        .await?;
    let assistant_message = response.message.clone();
    runtime
        .context_manager
        .add_assistant(Value::Object(assistant_message.clone()));
    Ok(AssistantStreamResult {
        message: assistant_message,
        finish_reason: response.finish_reason,
    })
}

async fn emit_finish_reason_notice(
    runtime: &TurnRuntime<'_>,
    finish_reason: Option<&str>,
) -> Result<()> {
    if !finish_reason_is_output_limit(finish_reason) {
        return Ok(());
    }
    let notice = output_limit_notice(runtime.model_client.config.max_tokens);
    runtime
        .activity
        .activity_box("Model output limit")
        .start_or_update("failed", &notice)
        .await
}

async fn inject_pending_watcher_content(runtime: &mut TurnRuntime<'_>) {
    let Some(watcher_runtime) = &runtime.watcher_runtime else {
        return;
    };
    for message in watcher_runtime.drain_pending_messages().await {
        runtime.context_manager.add_watcher_content(message);
    }
}

// ── Tool orchestration ─────────────────────────────────────────────────────

fn add_tool_results(
    context_manager: &mut ConversationContextManager,
    parsed_tool_calls: &[Value],
    tool_outputs: &[Value],
) {
    for (call, output) in parsed_tool_calls.iter().zip(tool_outputs.iter()) {
        let tool_call_id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let model_result = tool_output_for_model(output);
        context_manager.add_tool_result(&tool_call_id, &name, &model_result);
    }
}

/// Subagent sessions stash a dedicated `model_result` subtree so the LLM only
/// sees the stable slice of the payload. Mirror of
/// `_tool_output_for_model(output)`.
fn tool_output_for_model(output: &Value) -> Value {
    output
        .get("model_result")
        .and_then(|v| v.as_object().cloned())
        .map(Value::Object)
        .unwrap_or_else(|| output.clone())
}

async fn process_tool_calls(
    runtime: &mut TurnRuntime<'_>,
    parsed_tool_calls: &mut Vec<Value>,
) -> Result<Vec<Value>> {
    let mut current_mode = String::new();
    let mut indexed_outputs: Vec<Option<Value>> = vec![None; parsed_tool_calls.len()];
    let mut to_execute: Vec<(usize, Value)> = Vec::new();

    for (index, call) in parsed_tool_calls.iter().enumerate() {
        raise_if_cancelled(runtime.activity.cancel_event())?;
        let tool_call_id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let tool_name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let tool_args: Map<String, Value> = call
            .get("arguments")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let arg_parse_error = call
            .get(TOOL_ARG_PARSE_ERROR_FIELD)
            .and_then(Value::as_str)
            .map(str::to_string);

        runtime
            .activity
            .tool_call_pending(&tool_call_id, &tool_name, Value::Object(tool_args.clone()))
            .await?;

        if let Some(error) = arg_parse_error {
            let output = tool_arg_parse_error_output(&tool_name, &error);
            indexed_outputs[index] = Some(output.clone());
            runtime
                .activity
                .tool_call_update(
                    &tool_call_id,
                    "failed",
                    None,
                    None,
                    None,
                    Some(output),
                    None,
                )
                .await?;
            continue;
        }

        current_mode = (runtime.get_policy_mode)().await;

        let (allowed, next_mode, reason) = check_tool_permission(
            &runtime.activity,
            &tool_call_id,
            &tool_name,
            &tool_args,
            &current_mode,
            runtime.tool_policy.as_ref(),
        )
        .await?;
        current_mode = next_mode;

        if !allowed {
            let blocked_output = json!({
                "status": "blocked_by_policy",
                "mode": current_mode,
                "message": reason.unwrap_or_else(|| "Tool call blocked by session mode policy.".to_string()),
            });
            indexed_outputs[index] = Some(blocked_output.clone());
            runtime
                .activity
                .tool_call_update(
                    &tool_call_id,
                    "completed",
                    None,
                    None,
                    None,
                    Some(blocked_output),
                    None,
                )
                .await?;
            continue;
        }

        to_execute.push((index, call.clone()));
    }

    if !to_execute.is_empty() {
        let batch: Vec<Value> = to_execute
            .iter()
            .map(|(_, call)| {
                json!({
                    "tool_call_id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                    "name": call.get("name").and_then(Value::as_str).unwrap_or(""),
                    "arguments": call
                        .get("arguments")
                        .and_then(Value::as_object)
                        .cloned()
                        .map(Value::Object)
                        .unwrap_or_else(|| Value::Object(Map::new())),
                })
            })
            .collect();
        let ctx = runtime
            .activity
            .tool_execution_context(current_mode.clone());
        let batch_outputs = runtime
            .tool_manager
            .execute_tool_calls(batch, Some(&ctx))
            .await;
        for ((index, _), output) in to_execute.iter().zip(batch_outputs.into_iter()) {
            indexed_outputs[*index] = Some(output);
        }

        if runtime.activity.cancel_event().is_set() {
            let outputs: Vec<Value> = indexed_outputs
                .iter()
                .map(|slot| slot.clone().unwrap_or_else(cancelled_tool_output))
                .collect();
            return Err(anyhow::Error::new(TurnCancelled::with_outputs(outputs)));
        }
    }

    let finalized: Vec<Value> = indexed_outputs
        .into_iter()
        .map(|slot| {
            slot.unwrap_or_else(|| {
                json!({
                    "status": "completed_error",
                    "error": "Tool output missing due to interrupted execution.",
                })
            })
        })
        .collect();
    Ok(finalized)
}

fn annotate_tool_arg_parse_errors(
    calls: &mut [Value],
    finish_reason: Option<&str>,
    max_tokens: Option<u32>,
) {
    if !finish_reason_is_output_limit(finish_reason) {
        return;
    }
    let notice = output_limit_notice(max_tokens);
    for call in calls {
        let Some(obj) = call.as_object_mut() else {
            continue;
        };
        let Some(_error) = obj
            .get(TOOL_ARG_PARSE_ERROR_FIELD)
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        obj.insert(
            TOOL_ARG_PARSE_ERROR_FIELD.to_string(),
            Value::String(notice.clone()),
        );
    }
}

fn tool_arg_parse_error_output(_tool_name: &str, error: &str) -> Value {
    json!({
        "status": "completed_error",
        "error": error,
    })
}

fn finish_reason_is_output_limit(finish_reason: Option<&str>) -> bool {
    let Some(reason) = finish_reason else {
        return false;
    };
    matches!(
        reason.trim().to_ascii_lowercase().as_str(),
        "length" | "max_tokens" | "max_output_tokens"
    )
}

fn output_limit_notice(max_tokens: Option<u32>) -> String {
    match max_tokens {
        Some(limit) => format!(
            "Model output hit max_tokens={limit}; tool arguments were incomplete and likely truncated."
        ),
        None => "Model output hit its length limit; tool arguments were incomplete and likely truncated.".to_string(),
    }
}

async fn check_tool_permission(
    activity: &ActivityTurnHandle,
    tool_call_id: &str,
    tool_name: &str,
    tool_args: &Map<String, Value>,
    mode_id: &str,
    policy: &ToolPolicyConfig,
) -> Result<(bool, String, Option<String>)> {
    match resolve_tool_policy(mode_id, tool_name, tool_args, policy)? {
        ToolPolicyAction::Allow => return Ok((true, mode_id.to_string(), None)),
        ToolPolicyAction::Reject(reason) => {
            return Ok((false, mode_id.to_string(), Some(reason)));
        }
        ToolPolicyAction::Confirm => {}
    }

    let decision = activity
        .request_tool_permission(tool_call_id, tool_name, tool_args)
        .await?;

    let outcome = resolve_permission_decision(&decision)?;
    if decision == PERMISSION_CANCELLED {
        raise_if_cancelled(activity.cancel_event())?;
    }
    Ok((outcome.allowed, mode_id.to_string(), None))
}

fn raise_if_cancelled(cancel_event: &CancelEvent) -> Result<()> {
    if cancel_event.is_set() {
        return Err(anyhow::Error::new(TurnCancelled::new()));
    }
    Ok(())
}
