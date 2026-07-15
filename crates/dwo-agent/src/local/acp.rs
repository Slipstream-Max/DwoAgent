use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionCapabilities, SessionId,
    SessionInfo, SessionListCapabilities, SessionNotification, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Responder, on_receive_notification,
    on_receive_request,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use uuid::Uuid;

use super::ipc;

#[derive(Clone)]
struct AcpRuntime {
    config_path: PathBuf,
    observers: Arc<Mutex<HashMap<String, Arc<SessionObserver>>>>,
}

struct SessionObserver {
    endpoint_id: String,
    terminals: broadcast::Sender<TurnTerminal>,
}

#[derive(Clone, Debug)]
enum TurnTerminal {
    Completed(String),
    Cancelled(String),
    Failed { turn_id: String, error: String },
    StreamClosed,
}

impl TurnTerminal {
    fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Completed(turn_id) | Self::Cancelled(turn_id) => Some(turn_id),
            Self::Failed { turn_id, .. } => Some(turn_id),
            Self::StreamClosed => None,
        }
    }
}

pub async fn run(config_path: PathBuf) -> Result<()> {
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();
    let transport = ByteStreams::new(stdout, stdin);
    let runtime = AcpRuntime {
        config_path: config_path.clone(),
        observers: Arc::new(Mutex::new(HashMap::new())),
    };
    let new_config = config_path.clone();
    let list_config = config_path.clone();
    let load_runtime = runtime.clone();
    let prompt_runtime = runtime;
    let cancel_config = config_path;

    Agent
        .builder()
        .name("dwo")
        .on_receive_request(
            async move |request: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_info(Implementation::new("dwo", env!("CARGO_PKG_VERSION")))
                        .agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .prompt_capabilities(
                                    PromptCapabilities::new()
                                        .image(false)
                                        .audio(false)
                                        .embedded_context(true),
                                )
                                .session_capabilities(
                                    SessionCapabilities::new().list(SessionListCapabilities::new()),
                                ),
                        ),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                match ipc::request(
                    &new_config,
                    "session.new",
                    json!({"cwd": request.cwd, "title": Value::Null}),
                )
                .await
                {
                    Ok(value) => {
                        let id = value["session_id"].as_str().unwrap_or_default();
                        responder.respond(NewSessionResponse::new(SessionId::new(id)))
                    }
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest,
                        responder: Responder<ListSessionsResponse>,
                        _cx: ConnectionTo<Client>| {
                if request.cursor.is_some() {
                    return responder.respond(ListSessionsResponse::new(Vec::new()));
                }
                match ipc::request(&list_config, "session.list", json!({})).await {
                    Ok(value) => {
                        let records: Vec<dwo_agent_service::SessionRecord> =
                            serde_json::from_value(value).unwrap_or_default();
                        let cwd = request.cwd;
                        let sessions = records
                            .into_iter()
                            .filter(|record| cwd.as_ref().is_none_or(|cwd| record.info.cwd == *cwd))
                            .map(|record| {
                                SessionInfo::new(
                                    SessionId::new(record.info.id.as_str()),
                                    record.info.cwd,
                                )
                                .title(record.info.title)
                                .updated_at(record.info.updated_at_ms.to_string())
                            })
                            .collect();
                        responder.respond(ListSessionsResponse::new(sessions))
                    }
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let runtime = load_runtime.clone();
                cx.clone().spawn(async move {
                    match ensure_observer(&runtime, &request.session_id.to_string(), &cx, true)
                        .await
                    {
                        Ok(_) => responder.respond(LoadSessionResponse::new()),
                        Err(error) => responder.respond_with_error(internal_error(error)),
                    }?;
                    Ok(())
                })?;
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let runtime = prompt_runtime.clone();
                cx.clone().spawn(async move {
                    run_prompt(runtime, request, responder, cx).await;
                    Ok(())
                })?;
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _cx: ConnectionTo<Client>| {
                let _ = ipc::request(
                    &cancel_config,
                    "session.cancel",
                    json!({"session_id": notification.session_id}),
                )
                .await;
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(transport)
        .await
        .map_err(|error| anyhow::anyhow!("ACP connection failed: {error}"))
}

async fn run_prompt(
    runtime: AcpRuntime,
    request: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) {
    let session_id = request.session_id.to_string();
    let text = prompt_text(&request.prompt);
    let observer = match ensure_observer(&runtime, &session_id, &cx, false).await {
        Ok(observer) => observer,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let mut terminals = observer.terminals.subscribe();
    let prompt = match ipc::request(
        &runtime.config_path,
        "session.prompt",
        json!({
            "session_id": session_id,
            "endpoint_id": observer.endpoint_id,
            "message": text,
        }),
    )
    .await
    {
        Ok(prompt) => prompt,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let Some(turn_id) = prompt.get("turn_id").and_then(Value::as_str) else {
        let _ = responder.respond_with_error(internal_error("daemon omitted turn_id"));
        return;
    };

    loop {
        match terminals.recv().await {
            Ok(terminal) if terminal.turn_id().is_some_and(|id| id != turn_id) => continue,
            Ok(TurnTerminal::Completed(_)) => {
                let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                return;
            }
            Ok(TurnTerminal::Cancelled(_)) => {
                let _ = responder.respond(PromptResponse::new(StopReason::Cancelled));
                return;
            }
            Ok(TurnTerminal::Failed { error, .. }) => {
                let _ = responder.respond_with_error(internal_error(error));
                return;
            }
            Ok(TurnTerminal::StreamClosed) | Err(broadcast::error::RecvError::Closed) => {
                let _ = responder.respond_with_error(internal_error("session event stream closed"));
                return;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
        }
    }
}

async fn ensure_observer(
    runtime: &AcpRuntime,
    session_id: &str,
    cx: &ConnectionTo<Client>,
    replay: bool,
) -> Result<Arc<SessionObserver>> {
    let mut observers = runtime.observers.lock().await;
    if let Some(observer) = observers.get(session_id) {
        return Ok(observer.clone());
    }

    let endpoint_id = format!("acp-{}", Uuid::new_v4());
    let (snapshot, mut events) =
        ipc::subscribe(&runtime.config_path, session_id, &endpoint_id).await?;
    if replay {
        replay_snapshot(cx, session_id, &snapshot);
    }
    let (terminals, _) = broadcast::channel(32);
    let observer = Arc::new(SessionObserver {
        endpoint_id: endpoint_id.clone(),
        terminals,
    });
    observers.insert(session_id.to_string(), observer.clone());

    let observer_runtime = runtime.clone();
    let observer_session_id = session_id.to_string();
    let observer_endpoint_id = endpoint_id;
    let observer_state = observer.clone();
    let observer_cx = cx.clone();
    if let Err(error) = cx.spawn(async move {
        while let Some(frame) = events.recv().await {
            handle_session_event(
                &observer_runtime,
                &observer_cx,
                &observer_session_id,
                &observer_endpoint_id,
                &observer_state,
                frame,
            );
        }
        let _ = observer_state.terminals.send(TurnTerminal::StreamClosed);
        let mut observers = observer_runtime.observers.lock().await;
        if observers
            .get(&observer_session_id)
            .is_some_and(|current| current.endpoint_id == observer_endpoint_id)
        {
            observers.remove(&observer_session_id);
        }
        Ok(())
    }) {
        observers.remove(session_id);
        return Err(error.into());
    }
    Ok(observer)
}

fn handle_session_event(
    runtime: &AcpRuntime,
    cx: &ConnectionTo<Client>,
    session_id: &str,
    endpoint_id: &str,
    observer: &SessionObserver,
    frame: Value,
) {
    let Some(payload) = frame.get("params").and_then(|event| event.get("payload")) else {
        return;
    };
    let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "assistant_delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                send_chunk(cx, session_id, delta, false);
            }
        }
        "user_prompt_submitted" => {
            if let Some(content) = payload.get("content") {
                send_chunk(cx, session_id, &content_text(content), true);
            }
        }
        "tool_started" => send_tool_started(cx, session_id, payload),
        "tool_completed" => send_tool_completed(cx, session_id, payload),
        "permission_requested" => {
            let config_path = runtime.config_path.clone();
            let cx = cx.clone();
            let session_id = session_id.to_string();
            let endpoint_id = endpoint_id.to_string();
            let payload = payload.clone();
            let _ = cx.clone().spawn(async move {
                if let Err(error) =
                    resolve_permission(&config_path, &cx, &session_id, &endpoint_id, &payload).await
                {
                    eprintln!("ACP permission failed: {error:#}");
                }
                Ok(())
            });
        }
        "turn_completed" => send_terminal(payload, &observer.terminals, TurnTerminal::Completed),
        "turn_cancelled" => send_terminal(payload, &observer.terminals, TurnTerminal::Cancelled),
        "turn_failed" => {
            if let Some(turn_id) = payload.get("turn_id").and_then(Value::as_str) {
                let error = payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed")
                    .to_string();
                let _ = observer.terminals.send(TurnTerminal::Failed {
                    turn_id: turn_id.to_string(),
                    error,
                });
            }
        }
        _ => {}
    }
}

fn send_terminal(
    payload: &Value,
    terminals: &broadcast::Sender<TurnTerminal>,
    event: impl FnOnce(String) -> TurnTerminal,
) {
    if let Some(turn_id) = payload.get("turn_id").and_then(Value::as_str) {
        let _ = terminals.send(event(turn_id.to_string()));
    }
}

async fn resolve_permission(
    config_path: &Path,
    cx: &ConnectionTo<Client>,
    session_id: &str,
    endpoint_id: &str,
    payload: &Value,
) -> Result<()> {
    let permission = payload.get("permission").context("missing permission")?;
    let request_id = permission["request_id"]
        .as_str()
        .context("missing permission id")?;
    let tool_call_id: ToolCallId = permission["tool_call_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
        .into();
    let tool = ToolCallUpdate::new(
        tool_call_id,
        ToolCallUpdateFields::new()
            .title(
                permission["tool_name"]
                    .as_str()
                    .unwrap_or("tool")
                    .to_string(),
            )
            .status(ToolCallStatus::Pending),
    );
    let response = cx
        .send_request_to(
            Client,
            RequestPermissionRequest::new(
                SessionId::new(session_id),
                tool,
                vec![
                    PermissionOption::new(
                        "allow_once",
                        "Allow Once",
                        PermissionOptionKind::AllowOnce,
                    ),
                    PermissionOption::new(
                        "reject_once",
                        "Reject Once",
                        PermissionOptionKind::RejectOnce,
                    ),
                ],
            ),
        )
        .block_task()
        .await?;
    let allowed = matches!(
        response.outcome,
        RequestPermissionOutcome::Selected(ref selected)
            if selected.option_id.to_string() == "allow_once"
    );
    ipc::request(
        config_path,
        "session.permission",
        json!({
            "session_id": session_id,
            "endpoint_id": endpoint_id,
            "request_id": request_id,
            "allowed": allowed,
            "reason": if allowed { Value::Null } else { Value::String("denied by ACP client".to_string()) },
        }),
    )
    .await?;
    Ok(())
}

fn send_chunk(cx: &ConnectionTo<Client>, session_id: &str, text: &str, user: bool) {
    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
    let update = if user {
        SessionUpdate::UserMessageChunk(chunk)
    } else {
        SessionUpdate::AgentMessageChunk(chunk)
    };
    let _ = cx.send_notification_to(
        Client,
        SessionNotification::new(SessionId::new(session_id), update),
    );
}

fn send_tool_started(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let call = &payload["call"];
    let id: ToolCallId = call["tool_call_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
        .into();
    let tool = ToolCall::new(id, call["tool_name"].as_str().unwrap_or("tool").to_string())
        .kind(ToolKind::Other)
        .status(ToolCallStatus::InProgress);
    let _ = cx.send_notification_to(
        Client,
        SessionNotification::new(SessionId::new(session_id), SessionUpdate::ToolCall(tool)),
    );
}

fn send_tool_completed(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let result = &payload["result"];
    let id: ToolCallId = result["tool_call_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
        .into();
    let failed = result["output"]["status"]
        .as_str()
        .is_some_and(|status| matches!(status, "error" | "cancelled" | "blocked_by_policy"));
    let fields = ToolCallUpdateFields::new().status(if failed {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    });
    let _ = cx.send_notification_to(
        Client,
        SessionNotification::new(
            SessionId::new(session_id),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields)),
        ),
    );
}

fn replay_snapshot(cx: &ConnectionTo<Client>, session_id: &str, value: &Value) {
    let Some(transcript) = value
        .get("snapshot")
        .and_then(|snapshot| snapshot.get("record"))
        .and_then(|record| record.get("context"))
        .and_then(|context| context.get("transcript"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for item in transcript {
        match item.get("kind").and_then(Value::as_str) {
            Some("user") => send_chunk(cx, session_id, &content_text(&item["content"]), true),
            Some("assistant") => send_chunk(
                cx,
                session_id,
                item["content"].as_str().unwrap_or(""),
                false,
            ),
            _ => {}
        }
    }
}

fn prompt_text(prompt: &[ContentBlock]) -> String {
    let value = serde_json::to_value(prompt).unwrap_or(Value::Null);
    content_text(&value)
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(values) => values
            .iter()
            .map(content_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(values) => values
            .get("text")
            .or_else(|| values.get("content"))
            .map(content_text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn internal_error(error: impl std::fmt::Display) -> agent_client_protocol::schema::Error {
    agent_client_protocol::schema::Error::internal_error().data(error.to_string())
}
