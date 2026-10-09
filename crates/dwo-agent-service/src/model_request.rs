use std::time::Instant;

use dwo_context::TurnId;
use dwo_model_client::{
    ModelClientError, ModelReply, ModelSelection, ModelStreamEvent, error_kind, retry_info,
    wait_before_retry,
};
use serde_json::json;
use tokio::sync::mpsc;

use crate::compaction::compact;
use crate::events::{ActiveToolCall, CompactionTrigger, NotificationLevel};
use crate::session::ActorEvent;
use crate::turn::{ModelStep, TurnError, TurnExecution, TurnUpdate};

impl TurnExecution {
    pub(crate) async fn request(
        &mut self,
        model_step: &ModelStep,
    ) -> Result<ModelReply, TurnError> {
        let first = request_model(self, &model_step.selection).await;
        let original_error = match first {
            Ok(reply) => return Ok(reply),
            Err(error) => error,
        };

        if !is_context_length_exceeded(&original_error) {
            return Err(self.fail_bare(format!("{original_error:#}")));
        }

        let recovery = compact(
            self,
            model_step.selection.clone(),
            CompactionTrigger::Recovery,
        )
        .await;
        match recovery {
            Err(error) => Err(self.fail_bare(format!("{error:#}"))),
            Ok(false) => Err(self.fail_bare(format!("{original_error:#}"))),
            Ok(true) => {
                let retry = request_model(self, &model_step.selection).await;
                retry.map_err(|error| self.fail_bare(format!("{error:#}")))
            }
        }
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
        let (response, partial_content, partial_reasoning, progressed) = {
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
            let mut progressed = false;
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
                                &mut progressed,
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
                            &mut progressed,
                        );
                    }
                }
            };
            (response, partial_content, partial_reasoning, progressed)
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
                retry = next_retry_attempt(retry, progressed);
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
                turn.settle().await?;
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
                if !turn.steer.is_empty() {
                    turn.settle().await?;
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
    progressed: &mut bool,
) {
    // Any streamed event proves this attempt actually connected and produced
    // output, which counts as progress for the retry schedule.
    *progressed = true;
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

fn is_context_length_exceeded(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ModelClientError>()
        .is_some_and(ModelClientError::is_context_length_exceeded)
}

/// The retry counter tracks consecutive failures. A failed attempt that still
/// streamed output resolved its disconnect — the retry continues from the
/// partial content — so the next interruption is a new incident and starts a
/// fresh retry schedule; only attempts that fail without producing anything
/// keep escalating.
fn next_retry_attempt(retry: u32, progressed: bool) -> u32 {
    if progressed { 1 } else { retry + 1 }
}

#[cfg(test)]
mod tests {
    use super::next_retry_attempt;

    #[test]
    fn progress_restarts_the_retry_schedule() {
        assert_eq!(next_retry_attempt(0, false), 1);
        assert_eq!(next_retry_attempt(3, false), 4);
        assert_eq!(next_retry_attempt(3, true), 1);
        assert_eq!(next_retry_attempt(0, true), 1);
    }
}
