use std::sync::{Arc, Mutex};
use std::time::Instant;

use dwo_tools::{ExecutionContext, ToolEvent, ToolResult};
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
                    model: String::new(),
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
        let questionnaire_actor = self.actor.clone();
        let questionnaire_turn_id = self.turn_id.clone();
        execution.questionnaire = Some(Arc::new(move |request| {
            let actor = questionnaire_actor.clone();
            let turn_id = questionnaire_turn_id.clone();
            Box::pin(async move {
                let (completed, wait) = oneshot::channel();
                actor
                    .send(ActorEvent::Questionnaire {
                        turn_id,
                        request,
                        completed,
                    })
                    .map_err(|_| "session actor stopped".to_string())?;
                wait.await
                    .map_err(|_| "session actor dropped questionnaire request".to_string())?
            })
        }));
        let actor = self.actor.clone();
        let telemetry_turn_id = self.turn_id.clone();
        let started_terminals: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let telemetry_terminals = started_terminals.clone();
        execution.events = Some(Arc::new(move |event| {
            if let ToolEvent::TerminalOpened {
                tool_call_id,
                terminal_id,
                ..
            } = &event
                && let Ok(mut started) = telemetry_terminals.lock()
            {
                started.push((tool_call_id.clone(), terminal_id.to_string()));
            }
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
            _ = self.cancellation.cancelled() => {
                let started = started_terminals
                    .lock()
                    .map(|started| started.clone())
                    .unwrap_or_default();
                unrecorded_results(&parsed.local, &started)
            }
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

fn unrecorded_results(calls: &[Value], started_terminals: &[(String, String)]) -> Vec<ToolResult> {
    calls
        .iter()
        .map(|call| {
            let (tool_call_id, tool_name) = call_identity(call);
            let terminal_id = terminal_id_for(call, &tool_call_id, started_terminals);
            let mut output = json!({
                "tool": tool_name,
                "kind": "other",
                "status": "cancelled",
                "error": "turn cancelled",
                "note": "the operation may have executed; its result was not recorded",
            });
            if let Some(terminal_id) = &terminal_id {
                output["terminal_id"] = json!(terminal_id);
            }
            ToolResult {
                tool_call_id,
                tool_name: tool_name.clone(),
                output,
                model: unrecorded_model_note(&tool_name, terminal_id.as_deref()),
                model_context: Vec::new(),
            }
        })
        .collect()
}

fn terminal_id_for(
    call: &Value,
    tool_call_id: &str,
    started_terminals: &[(String, String)],
) -> Option<String> {
    if let Some((_, terminal_id)) = started_terminals
        .iter()
        .rev()
        .find(|(call_id, _)| call_id == tool_call_id)
    {
        return Some(terminal_id.clone());
    }
    let arguments = call.get("arguments")?;
    match arguments {
        Value::Object(object) => object
            .get("terminal_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()?
            .get("terminal_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn unrecorded_model_note(tool_name: &str, terminal_id: Option<&str>) -> String {
    match (tool_name, terminal_id) {
        ("terminal", Some(terminal_id)) => format!(
            "The turn was cancelled while this terminal operation was pending; it may have already run. Check terminal {terminal_id} for its current state before repeating it."
        ),
        ("terminal", None) => "The turn was cancelled while this terminal operation was pending; it may have already run. Check the terminal state before repeating it.".to_string(),
        ("file_edit", _) => "The turn was cancelled while this file_edit was pending; the patch may have already been applied but its result was not recorded. Check the affected files before sending another patch.".to_string(),
        ("read_file", _) => "The turn was cancelled before this read completed; no result was recorded.".to_string(),
        _ => "The turn was cancelled while this operation was pending; it may have already executed but its result was not recorded. Verify the current state before repeating it.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrecorded_terminal_names_the_started_terminal() {
        let call = json!({"id":"call-1","name":"terminal","arguments":{"command":"echo hi"}});
        let started = vec![("call-1".to_string(), "term-9".to_string())];
        let results = unrecorded_results(&[call], &started);
        assert_eq!(results[0].tool_name, "terminal");
        assert_eq!(results[0].output["status"], "cancelled");
        assert_eq!(results[0].output["terminal_id"], "term-9");
        assert!(results[0].model.contains("term-9"));
    }

    #[test]
    fn unrecorded_terminal_input_reuses_the_target_terminal() {
        let call = json!({"id":"call-2","name":"terminal","arguments":{"terminal_id":"term-3"}});
        let results = unrecorded_results(&[call], &[]);
        assert_eq!(results[0].output["terminal_id"], "term-3");
        assert!(results[0].model.contains("term-3"));
    }

    #[test]
    fn unrecorded_file_edit_asks_to_check_the_files() {
        let call = json!({"id":"call-3","name":"file_edit","arguments":{"patch":"*** Begin Patch\n*** End Patch"}});
        let results = unrecorded_results(&[call], &[]);
        assert_eq!(
            results[0].output["note"],
            "the operation may have executed; its result was not recorded"
        );
        assert!(results[0].model.contains("may have already been applied"));
    }

    #[test]
    fn unrecorded_read_file_is_harmless() {
        let call = json!({"id":"call-4","name":"read_file","arguments":{"path":"a.txt"}});
        let results = unrecorded_results(&[call], &[]);
        assert!(results[0].model.contains("no result was recorded"));
    }
}
