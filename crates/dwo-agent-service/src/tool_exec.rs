use std::sync::Arc;
use std::time::Instant;

use dwo_tools::{ExecutionContext, ToolResult};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::session::ActorEvent;
use crate::tool_calls::{ParsedToolCalls, call_identity};
use crate::turn::{ModelStep, TurnError, TurnExecution, TurnUpdate};

impl TurnExecution {
    pub(crate) fn replay_remote_tool_calls(&self, parsed: &ParsedToolCalls) {
        for call in &parsed.remote_display {
            let mut started = call.clone();
            started.status = "in_progress".to_string();
            self.emit(TurnUpdate::ToolChanged {
                turn_id: self.turn_id.clone(),
                call: started,
            });
            let raw_output = parsed
                .remote
                .iter()
                .find(|raw| raw.get("id").and_then(Value::as_str) == Some(&call.tool_call_id))
                .and_then(|raw| raw.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            self.emit(TurnUpdate::ToolCompleted {
                turn_id: self.turn_id.clone(),
                result: ToolResult {
                    tool_call_id: call.tool_call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    output: raw_output,
                    model_context: Vec::new(),
                },
            });
        }
    }

    pub(crate) async fn run_tool_batch(
        &mut self,
        model_step: &ModelStep,
        parsed: ParsedToolCalls,
    ) -> Result<(), TurnError> {
        for mut call in parsed
            .local_display
            .into_iter()
            .filter(|call| call.tool_name != "todo")
        {
            call.status = "in_progress".to_string();
            self.emit(TurnUpdate::ToolChanged {
                turn_id: self.turn_id.clone(),
                call,
            });
        }

        let mut execution = ExecutionContext::new(self.config.borrow().mode);
        execution.confirmation = Some(self.permission.confirmation_handler());
        execution.allow_image_input = model_step.allow_image_input;
        let todo_actor = self.actor.clone();
        let todo_turn_id = self.turn_id.clone();
        execution.todo = Some(Arc::new(move |request| {
            let actor = todo_actor.clone();
            let turn_id = todo_turn_id.clone();
            Box::pin(async move {
                let (completed, wait) = oneshot::channel();
                actor
                    .send(ActorEvent::Todo {
                        turn_id,
                        request,
                        completed,
                    })
                    .map_err(|_| "session actor stopped".to_string())?;
                wait.await
                    .map_err(|_| "session actor dropped todo request".to_string())?
            })
        }));
        let actor = self.actor.clone();
        let telemetry_turn_id = self.turn_id.clone();
        execution.events = Some(Arc::new(move |event| {
            let _ = actor.send(ActorEvent::Turn(TurnUpdate::ToolTelemetry {
                turn_id: telemetry_turn_id.clone(),
                event,
            }));
        }));

        let tool_count = parsed.local.len();
        let tools_started = Instant::now();
        tracing::info!(
            event = "tool.batch_started",
            session_id = %self.session_id,
            turn_id = %self.turn_id,
            tool_count,
            "tool batch started"
        );
        let tool_results = tokio::select! {
            _ = self.cancellation.cancelled() => cancelled_results(&parsed.local),
            results = self.tools.execute_batch(parsed.local.clone(), &execution) => results,
        };

        let context_results = tool_results
            .iter()
            .map(ToolResult::context_record)
            .collect::<Vec<_>>();
        for result in &tool_results {
            tracing::info!(
                event = "tool.call_completed",
                session_id = %self.session_id,
                turn_id = %self.turn_id,
                tool_call_id = %result.tool_call_id,
                tool = %result.tool_name,
                status = result.output.get("status").and_then(|value| value.as_str()),
                "tool call completed"
            );
            if result.tool_name != "todo" {
                self.emit(TurnUpdate::ToolCompleted {
                    turn_id: self.turn_id.clone(),
                    result: result.clone(),
                });
            }
        }
        tracing::info!(
            event = "tool.batch_completed",
            session_id = %self.session_id,
            turn_id = %self.turn_id,
            tool_count = tool_results.len(),
            duration_ms = tools_started.elapsed().as_millis() as u64,
            "tool batch completed"
        );
        self.context.append_tool_batch(context_results);

        let settled = self.settle().await;
        settled.map_err(|error| self.fail("persist checkpoint", format!("{error:#}")))?;

        if self.cancellation.is_cancelled() {
            return Err(TurnError::Cancelled);
        }
        Ok(())
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
                    "kind": "other",
                    "status": "cancelled",
                    "error": "turn cancelled",
                }),
                model_context: Vec::new(),
            }
        })
        .collect()
}
