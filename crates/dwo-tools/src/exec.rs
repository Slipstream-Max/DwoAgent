use std::future::Future;
use std::pin::Pin;

use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};

use crate::ToolEvent;
use crate::call::{ParsedToolCall, TerminalArgs, ToolCall};
use crate::manager::{ConfirmationRequest, ExecutionContext, ToolManager};
use crate::policy::Authorization;
use crate::result::ToolResult;
use crate::terminal::TerminalTelemetry;

pub(crate) async fn execute(
    manager: &ToolManager,
    call: ParsedToolCall,
    context: &ExecutionContext,
) -> ToolResult {
    let id = call.id.clone();
    let name = call.call.name().to_string();
    match manager.policy.authorize(context.mode, &call.call.intent()) {
        Authorization::Allow => {}
        Authorization::Deny(reason) => {
            return blocked_result(&id, &name, reason);
        }
        Authorization::Confirm => {
            let Some(confirm) = &context.confirmation else {
                return result_error(
                    &id,
                    &name,
                    "Tool call requires confirmation, but no confirmation handler is available.",
                );
            };
            let decision = confirm(ConfirmationRequest {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                arguments: call.raw_arguments.clone(),
            })
            .await;
            if !decision.allowed {
                return blocked_result(
                    &id,
                    &name,
                    decision
                        .reason
                        .unwrap_or_else(|| "Tool call was not approved.".to_string()),
                );
            }
        }
    }

    let mut model_context = Vec::new();
    let output = match call.call {
        ToolCall::Terminal(TerminalArgs::Run {
            command,
            yield_ms,
            timeout_ms,
        }) => manager
            .terminals
            .run_with_events(
                command,
                yield_ms,
                timeout_ms,
                Some(TerminalTelemetry::new(id.clone(), context.events.clone())),
            )
            .await
            .map(terminal_output),
        ToolCall::Terminal(TerminalArgs::Input {
            terminal_id,
            data,
            yield_ms,
        }) => manager
            .terminals
            .input(&terminal_id, &data, yield_ms)
            .await
            .map(terminal_output),
        ToolCall::Terminal(TerminalArgs::Kill { terminal_id }) => manager
            .terminals
            .kill(&terminal_id)
            .await
            .map(terminal_output),
        ToolCall::FileEdit(args) => manager
            .file_edit
            .execute(args.patch, manager.cwd.clone())
            .await
            .map(|result| {
                emit(
                    context,
                    ToolEvent::FileChanged {
                        tool_call_id: id.clone(),
                        changes: result.changes.clone(),
                        patch: result.patch,
                    },
                );
                json!({
                    "tool": "file_edit",
                    "kind": "other",
                    "status": "completed",
                    "changes": result.changes,
                })
            }),
        ToolCall::ReadFile(args) => {
            let path = if args.path.is_absolute() {
                args.path.clone()
            } else {
                manager.cwd.join(&args.path)
            };
            crate::read_file::execute(args, &manager.cwd, context.allow_image_input)
                .await
                .map(|result| {
                    emit(
                        context,
                        ToolEvent::FileRead {
                            tool_call_id: id.clone(),
                            path: std::fs::canonicalize(&path).unwrap_or(path),
                        },
                    );
                    model_context = result.model_context;
                    result.output
                })
        }
        ToolCall::Handoff(args) => Ok(json!({
            "tool": "handoff",
            "kind": "other",
            "status": "completed",
            "handoff_text": args.text,
        })),
        ToolCall::Plan(request) => {
            let Some(handler) = &context.plan else {
                return result_error(&id, &name, "plan handler is not available");
            };
            handler(request)
                .await
                .and_then(|response| {
                    serde_json::to_value(response).map_err(|error| error.to_string())
                })
                .map_err(anyhow::Error::msg)
        }
    };

    ToolResult {
        tool_call_id: id,
        tool_name: name.clone(),
        output: output.unwrap_or_else(|error| error_output(&name, format!("{error:#}"))),
        model_context,
    }
}

fn emit(context: &ExecutionContext, event: ToolEvent) {
    if let Some(events) = &context.events {
        events(event);
    }
}

pub(crate) async fn execute_batch(
    manager: &ToolManager,
    raw_calls: Vec<Value>,
    context: &ExecutionContext,
) -> Vec<ToolResult> {
    let mut outputs: Vec<Option<ToolResult>> = vec![None; raw_calls.len()];
    let mut terminal_calls = Vec::new();
    let mut file_calls = Vec::new();
    let parsed_calls = raw_calls
        .into_iter()
        .map(ParsedToolCall::parse)
        .collect::<Vec<_>>();
    let handoff_count = parsed_calls
        .iter()
        .filter(|parsed| matches!(parsed, Ok(call) if matches!(call.call, ToolCall::Handoff(_))))
        .count();
    if handoff_count > 0 && parsed_calls.len() > 1 {
        return parsed_calls
            .into_iter()
            .map(|parsed| match parsed {
                Ok(call) => result_error_with_code(
                    &call.id,
                    call.call.name(),
                    "handoff_must_be_only_tool",
                    "handoff must be the only tool call in a batch.",
                ),
                Err(error) => result_error(&error.id, &error.name, error.message),
            })
            .collect();
    }
    let file_edit_count = parsed_calls
        .iter()
        .filter(|parsed| match parsed {
            Ok(call) => matches!(call.call, ToolCall::FileEdit(_)),
            Err(error) => error.name == "file_edit",
        })
        .count();

    for (index, parsed) in parsed_calls.into_iter().enumerate() {
        match parsed {
            Ok(call) if matches!(call.call, ToolCall::FileEdit(_)) && file_edit_count > 1 => {
                outputs[index] = Some(result_error_with_code(
                    &call.id,
                    call.call.name(),
                    "multiple_file_edit_calls",
                    "Only one file_edit call is allowed per tool batch. Combine related file operations into one patch or issue later edits in a subsequent assistant response.",
                ));
            }
            Ok(call) if matches!(call.call, ToolCall::FileEdit(_)) => {
                file_calls.push((index, call));
            }
            Ok(call) => terminal_calls.push((index, call)),
            Err(error) => {
                outputs[index] = Some(if error.name == "file_edit" && file_edit_count > 1 {
                    result_error_with_code(
                        &error.id,
                        &error.name,
                        "multiple_file_edit_calls",
                        format!(
                            "Only one file_edit call is allowed per tool batch. Original call error: {}",
                            error.message
                        ),
                    )
                } else {
                    result_error(&error.id, &error.name, error.message)
                });
            }
        }
    }

    type BatchFuture<'a> = Pin<Box<dyn Future<Output = Vec<(usize, ToolResult)>> + Send + 'a>>;
    let mut pending: FuturesUnordered<BatchFuture<'_>> = FuturesUnordered::new();
    for (index, call) in terminal_calls {
        pending.push(Box::pin(async move {
            vec![(index, execute(manager, call, context).await)]
        }));
    }
    if !file_calls.is_empty() {
        pending.push(Box::pin(async move {
            let mut completed = Vec::with_capacity(file_calls.len());
            for (index, call) in file_calls {
                completed.push((index, execute(manager, call, context).await));
            }
            completed
        }));
    }
    while let Some(completed) = pending.next().await {
        for (index, output) in completed {
            outputs[index] = Some(output);
        }
    }

    outputs
        .into_iter()
        .map(|output| output.expect("every parsed or rejected call produces a result"))
        .collect()
}

fn terminal_output(snapshot: crate::terminal::TerminalSnapshot) -> Value {
    json!({
        "tool": "terminal",
        "kind": "other",
        "terminal_id": snapshot.terminal_id,
        "status": snapshot.status,
        "exit_code": snapshot.exit_code,
        "output": snapshot.output,
        "command": snapshot.command,
        "cwd": snapshot.cwd,
    })
}

fn blocked_result(id: &str, name: &str, message: impl Into<String>) -> ToolResult {
    ToolResult {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        output: json!({
            "tool": name,
            "kind": tool_kind(name),
            "status": "blocked_by_policy",
            "message": message.into(),
        }),
        model_context: Vec::new(),
    }
}

fn result_error(id: &str, name: &str, message: impl Into<String>) -> ToolResult {
    ToolResult {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        output: error_output(name, message),
        model_context: Vec::new(),
    }
}

fn result_error_with_code(
    id: &str,
    name: &str,
    code: &str,
    message: impl Into<String>,
) -> ToolResult {
    ToolResult {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        output: json!({
            "tool": name,
            "kind": tool_kind(name),
            "status": "error",
            "code": code,
            "error": message.into(),
        }),
        model_context: Vec::new(),
    }
}

fn error_output(name: &str, message: impl Into<String>) -> Value {
    json!({
        "tool": name,
        "kind": tool_kind(name),
        "status": "error",
        "error": message.into(),
    })
}

fn tool_kind(name: &str) -> &'static str {
    let _ = name;
    "other"
}
