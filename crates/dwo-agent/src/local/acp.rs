use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::{
    AgentCapabilities, CancelNotification, ConfigOptionUpdate, ContentBlock, ContentChunk,
    EmbeddedResourceResource, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptCapabilities, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, ResourceLink, SessionCapabilities, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionId, SessionInfo,
    SessionInfoUpdate, SessionListCapabilities, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, TextContent,
    ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind, UsageUpdate,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Responder, on_receive_notification,
    on_receive_request,
};
use anyhow::{Context, Result};
use serde::Deserialize;
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
    let prompt_runtime = runtime.clone();
    let set_runtime = runtime;
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
                        cx: ConnectionTo<Client>| {
                match ipc::request(
                    &new_config,
                    "session.new",
                    json!({"cwd": request.cwd, "title": Value::Null}),
                )
                .await
                {
                    Ok(value) => {
                        let id = value["session_id"].as_str().unwrap_or_default();
                        match session_config_options(&new_config, id).await {
                            Ok(options) => {
                                let result = responder.respond(
                                    NewSessionResponse::new(SessionId::new(id))
                                        .config_options(options),
                                );
                                if let Some((used, size)) = snapshot_usage(&value) {
                                    send_usage_update(&cx, id, used, size);
                                }
                                result
                            }
                            Err(error) => responder.respond_with_error(internal_error(error)),
                        }
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
                        Ok(_) => match session_config_options(
                            &runtime.config_path,
                            &request.session_id.to_string(),
                        )
                        .await
                        {
                            Ok(options) => responder
                                .respond(LoadSessionResponse::new().config_options(options)),
                            Err(error) => responder.respond_with_error(internal_error(error)),
                        },
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
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        cx: ConnectionTo<Client>| {
                let session_id = request.session_id.to_string();
                let config_id = request.config_id.to_string();
                let value = request.value.to_string();
                match ipc::request(
                    &set_runtime.config_path,
                    "session.set_config_option",
                    json!({
                        "session_id": session_id,
                        "config_id": config_id,
                        "value": value,
                    }),
                )
                .await
                {
                    Ok(value) => {
                        match session_config_options(&set_runtime.config_path, &session_id).await {
                            Ok(options) => {
                                let result =
                                    responder.respond(SetSessionConfigOptionResponse::new(options));
                                let observed =
                                    set_runtime.observers.lock().await.contains_key(&session_id);
                                if config_id == "model"
                                    && !observed
                                    && let Some((used, size)) = snapshot_usage(&value)
                                {
                                    send_usage_update(&cx, &session_id, used, size);
                                }
                                result
                            }
                            Err(error) => responder.respond_with_error(internal_error(error)),
                        }
                    }
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
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
    let text = match prompt_text(&request.prompt) {
        Ok(text) => text,
        Err(error) => {
            let _ = responder.respond_with_error(invalid_params(error));
            return;
        }
    };
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
    if let Some(observer) = observers.get(session_id).cloned() {
        drop(observers);
        if replay {
            let snapshot = ipc::request(
                &runtime.config_path,
                "session.snapshot",
                json!({"session_id": session_id}),
            )
            .await?;
            replay_snapshot(cx, session_id, &json!({"snapshot": snapshot}));
        }
        return Ok(observer);
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
        "assistant_reasoning_delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                send_thought_chunk(cx, session_id, delta);
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
        "config_changed" => {
            let config_path = runtime.config_path.clone();
            let cx = cx.clone();
            let session_id = session_id.to_string();
            let _ = cx.clone().spawn(async move {
                match session_config_options(&config_path, &session_id).await {
                    Ok(options) => {
                        let _ = cx.send_notification_to(
                            Client,
                            SessionNotification::new(
                                SessionId::new(session_id),
                                SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options)),
                            ),
                        );
                    }
                    Err(error) => eprintln!("refresh ACP config options: {error:#}"),
                }
                Ok(())
            });
        }
        "usage_changed" => {
            if let (Some(used), Some(size)) = (
                payload.get("used").and_then(Value::as_u64),
                payload.get("size").and_then(Value::as_u64),
            ) {
                send_usage_update(cx, session_id, used, size);
            }
        }
        "title_changed" => {
            if let Some(title) = payload.get("title").and_then(Value::as_str) {
                send_session_info_update(
                    cx,
                    session_id,
                    title,
                    payload.get("updated_at_ms").and_then(Value::as_u64),
                );
            }
        }
        _ => {}
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionOptionSnapshot {
    config: dwo_agent_service::SessionConfig,
    models: Vec<SessionModelOption>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionModelOption {
    id: String,
    reasoning: Vec<String>,
    default_reasoning: String,
}

async fn session_config_options(
    config_path: &Path,
    session_id: &str,
) -> Result<Vec<SessionConfigOption>> {
    let value = ipc::request(
        config_path,
        "session.options",
        json!({"session_id": session_id}),
    )
    .await?;
    let snapshot: SessionOptionSnapshot = serde_json::from_value(value)?;
    build_session_config_options(snapshot)
}

fn build_session_config_options(
    snapshot: SessionOptionSnapshot,
) -> Result<Vec<SessionConfigOption>> {
    let model = snapshot
        .models
        .iter()
        .find(|model| model.id == snapshot.config.model)
        .with_context(|| format!("session model disappeared: {}", snapshot.config.model))?;
    let reasoning = snapshot
        .config
        .reasoning
        .clone()
        .unwrap_or_else(|| model.default_reasoning.clone());
    let reasoning_options = model.reasoning.clone();
    let policy = match snapshot.config.mode {
        dwo_tools::SessionMode::FullAccess => "full_access",
        dwo_tools::SessionMode::Confirm => "confirm",
        dwo_tools::SessionMode::Watch => "watch",
    };

    Ok(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            snapshot.config.model.clone(),
            snapshot
                .models
                .iter()
                .map(|model| SessionConfigSelectOption::new(model.id.clone(), model.id.clone()))
                .collect::<Vec<_>>(),
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "reasoning_mode",
            "Reasoning",
            reasoning,
            reasoning_options
                .into_iter()
                .map(|mode| SessionConfigSelectOption::new(mode.clone(), mode))
                .collect::<Vec<_>>(),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
        SessionConfigOption::select(
            "policy_mode",
            "Policy",
            policy,
            vec![
                SessionConfigSelectOption::new("full_access", "Full Access"),
                SessionConfigSelectOption::new("confirm", "Confirm"),
                SessionConfigSelectOption::new("watch", "Watch"),
            ],
        )
        .category(SessionConfigOptionCategory::Mode),
    ])
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

fn send_thought_chunk(cx: &ConnectionTo<Client>, session_id: &str, text: &str) {
    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
    let _ = cx.send_notification_to(
        Client,
        SessionNotification::new(
            SessionId::new(session_id),
            SessionUpdate::AgentThoughtChunk(chunk),
        ),
    );
}

fn send_session_info_update(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    title: &str,
    updated_at_ms: Option<u64>,
) {
    let update = session_info_update(title, updated_at_ms);
    let _ = cx.send_notification_to(
        Client,
        SessionNotification::new(SessionId::new(session_id), update),
    );
}

fn session_info_update(title: &str, updated_at_ms: Option<u64>) -> SessionUpdate {
    let mut update = SessionInfoUpdate::new().title(title.to_string());
    if let Some(updated_at_ms) = updated_at_ms {
        update = update.updated_at(updated_at_ms.to_string());
    }
    SessionUpdate::SessionInfoUpdate(update)
}

fn send_usage_update(cx: &ConnectionTo<Client>, session_id: &str, used: u64, size: u64) {
    let _ = cx.send_notification_to(
        Client,
        SessionNotification::new(SessionId::new(session_id), usage_update(used, size)),
    );
}

fn usage_update(used: u64, size: u64) -> SessionUpdate {
    SessionUpdate::UsageUpdate(UsageUpdate::new(used, size))
}

fn send_tool_started(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let tool = tool_started(payload);
    let _ = cx.send_notification_to(
        Client,
        SessionNotification::new(SessionId::new(session_id), SessionUpdate::ToolCall(tool)),
    );
}

fn send_tool_completed(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let update = tool_completed(payload);
    let _ = cx.send_notification_to(
        Client,
        SessionNotification::new(
            SessionId::new(session_id),
            SessionUpdate::ToolCallUpdate(update),
        ),
    );
}

fn tool_started(payload: &Value) -> ToolCall {
    let call = &payload["call"];
    let id: ToolCallId = call["tool_call_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
        .into();
    let name = call["tool_name"].as_str().unwrap_or("tool");
    let raw_input = call.get("raw_input").cloned().unwrap_or(Value::Null);
    let mut tool = ToolCall::new(id, name.to_string())
        .kind(tool_kind())
        .status(ToolCallStatus::InProgress)
        .raw_input(raw_input.clone());
    if let Some(text) = render_tool_input(name, &raw_input) {
        tool = tool.content(text_content(text));
    }
    tool
}

fn tool_completed(payload: &Value) -> ToolCallUpdate {
    let result = &payload["result"];
    let id: ToolCallId = result["tool_call_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
        .into();
    let failed = result["output"]["status"]
        .as_str()
        .is_some_and(|status| matches!(status, "error" | "cancelled" | "blocked_by_policy"));
    let output = result.get("output").cloned().unwrap_or(Value::Null);
    let mut fields = ToolCallUpdateFields::new()
        .status(if failed {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        })
        .raw_output(output.clone());
    if let Some(text) = render_tool_output(&output) {
        fields = fields.content(text_content(text));
    }
    ToolCallUpdate::new(id, fields)
}

fn tool_kind() -> ToolKind {
    ToolKind::Other
}

fn render_tool_input(name: &str, input: &Value) -> Option<String> {
    let preferred = match name {
        "terminal" => input.get("command").and_then(Value::as_str),
        "file_edit" => input.get("patch").and_then(Value::as_str),
        _ => None,
    };
    preferred
        .map(str::to_string)
        .or_else(|| pretty_nonempty(input))
        .map(|text| truncate_tool_text(&text))
}

fn render_tool_output(output: &Value) -> Option<String> {
    output
        .get("output")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| pretty_nonempty(output))
        .map(|text| truncate_tool_text(&text))
}

fn pretty_nonempty(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    Some(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
}

fn truncate_tool_text(text: &str) -> String {
    const LIMIT: usize = 8_000;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let mut truncated = text.chars().take(LIMIT).collect::<String>();
    truncated.push_str("\n[TRUNCATED]");
    truncated
}

fn text_content(text: String) -> Vec<ToolCallContent> {
    vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
        text,
    )))]
}

fn replay_snapshot(cx: &ConnectionTo<Client>, session_id: &str, value: &Value) {
    let Some(snapshot) = value.get("snapshot") else {
        return;
    };
    if let Some((used, size)) = snapshot_usage(snapshot) {
        send_usage_update(cx, session_id, used, size);
    }
    let Some(record) = snapshot.get("record") else {
        return;
    };
    if let Some(title) = record
        .get("info")
        .and_then(|info| info.get("title"))
        .and_then(Value::as_str)
    {
        let updated_at_ms = record
            .get("info")
            .and_then(|info| info.get("updated_at_ms"))
            .and_then(Value::as_u64);
        send_session_info_update(cx, session_id, title, updated_at_ms);
    }
    let Some(transcript) = snapshot.get("transcript").and_then(Value::as_array) else {
        return;
    };
    for event in transcript {
        let Some(payload) = event.get("payload") else {
            continue;
        };
        match payload.get("kind").and_then(Value::as_str) {
            Some("user_prompt_submitted") => {
                send_chunk(cx, session_id, &content_text(&payload["content"]), true)
            }
            Some("assistant_delta") => {
                if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                    send_chunk(cx, session_id, delta, false);
                }
            }
            Some("assistant_reasoning_delta") => {
                if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                    send_thought_chunk(cx, session_id, delta);
                }
            }
            Some("tool_started") => send_tool_started(cx, session_id, payload),
            Some("tool_completed") => send_tool_completed(cx, session_id, payload),
            _ => {}
        }
    }
}

fn snapshot_usage(snapshot: &Value) -> Option<(u64, u64)> {
    let usage = snapshot.get("usage")?;
    Some((usage.get("used")?.as_u64()?, usage.get("size")?.as_u64()?))
}

fn prompt_text(prompt: &[ContentBlock]) -> Result<String> {
    let mut output = Vec::with_capacity(prompt.len());
    for block in prompt {
        let text = match block {
            ContentBlock::Text(content) => content.text.clone(),
            ContentBlock::ResourceLink(resource) => resource_link_text(resource),
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(resource) => embedded_resource_text(
                    &resource.uri,
                    resource.mime_type.as_deref(),
                    &resource.text,
                ),
                EmbeddedResourceResource::BlobResourceContents(_) => {
                    anyhow::bail!("binary embedded resources are not supported")
                }
                _ => anyhow::bail!("unsupported embedded resource type"),
            },
            ContentBlock::Image(_) => anyhow::bail!("image input is not supported"),
            ContentBlock::Audio(_) => anyhow::bail!("audio input is not supported"),
            _ => anyhow::bail!("unsupported ACP content block"),
        };
        if !text.is_empty() {
            output.push(text);
        }
    }
    Ok(output.join("\n"))
}

fn resource_link_text(resource: &ResourceLink) -> String {
    let mut text = format!(
        "Referenced resource:\nName: {}\nURI: {}",
        resource.name, resource.uri
    );
    if let Some(title) = &resource.title {
        text.push_str(&format!("\nTitle: {title}"));
    }
    if let Some(mime_type) = &resource.mime_type {
        text.push_str(&format!("\nMIME type: {mime_type}"));
    }
    if let Some(size) = resource.size {
        text.push_str(&format!("\nSize: {size} bytes"));
    }
    if let Some(description) = &resource.description {
        text.push_str(&format!("\nDescription: {description}"));
    }
    text
}

fn embedded_resource_text(uri: &str, mime_type: Option<&str>, content: &str) -> String {
    let mime_type = mime_type.unwrap_or("text/plain");
    format!("Embedded resource:\nURI: {uri}\nMIME type: {mime_type}\nContent:\n{content}")
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

fn invalid_params(error: impl std::fmt::Display) -> agent_client_protocol::schema::Error {
    agent_client_protocol::schema::Error::invalid_params().data(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        BlobResourceContents, EmbeddedResource, ImageContent, TextResourceContents,
    };

    #[test]
    fn acp_prompt_flattens_text_embedded_text_and_resource_links_in_order() {
        let prompt = vec![
            ContentBlock::Text(TextContent::new("explain")),
            ContentBlock::ResourceLink(
                ResourceLink::new("README.md", "file:///C:/workspace/README.md")
                    .mime_type("text/markdown")
                    .description("Project documentation"),
            ),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                    "fn main() {}",
                    "file:///C:/workspace/main.rs",
                )),
            )),
        ];

        assert_eq!(
            prompt_text(&prompt).unwrap(),
            "explain\nReferenced resource:\nName: README.md\nURI: file:///C:/workspace/README.md\nMIME type: text/markdown\nDescription: Project documentation\nEmbedded resource:\nURI: file:///C:/workspace/main.rs\nMIME type: text/plain\nContent:\nfn main() {}"
        );
    }

    #[test]
    fn acp_prompt_rejects_images_and_binary_embedded_resources() {
        let image = [ContentBlock::Image(ImageContent::new(
            "aGVsbG8=",
            "image/png",
        ))];
        assert_eq!(
            prompt_text(&image).unwrap_err().to_string(),
            "image input is not supported"
        );

        let blob = [ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(BlobResourceContents::new(
                "JVBERi0=",
                "attachment://report.pdf",
            )),
        ))];
        assert_eq!(
            prompt_text(&blob).unwrap_err().to_string(),
            "binary embedded resources are not supported"
        );
    }

    #[test]
    fn acp_exposes_model_reasoning_and_policy_options() {
        let options = build_session_config_options(SessionOptionSnapshot {
            config: dwo_agent_service::SessionConfig {
                mode: dwo_tools::SessionMode::FullAccess,
                model: "deepseek-v4-flash".to_string(),
                reasoning: Some("max".to_string()),
            },
            models: vec![SessionModelOption {
                id: "deepseek-v4-flash".to_string(),
                reasoning: vec!["high".to_string(), "max".to_string()],
                default_reasoning: "high".to_string(),
            }],
        })
        .unwrap();
        let json = serde_json::to_value(options).unwrap();
        assert_eq!(json[0]["id"], "model");
        assert_eq!(json[0]["currentValue"], "deepseek-v4-flash");
        assert_eq!(json[1]["id"], "reasoning_mode");
        assert_eq!(json[1]["currentValue"], "max");
        assert_eq!(json[2]["id"], "policy_mode");
        assert_eq!(json[2]["currentValue"], "full_access");
    }

    #[test]
    fn title_change_maps_to_acp_session_info_update() {
        let update =
            serde_json::to_value(session_info_update("Generated title", Some(42))).unwrap();
        assert_eq!(update["sessionUpdate"], "session_info_update");
        assert_eq!(update["title"], "Generated title");
        assert_eq!(update["updatedAt"], "42");
    }

    #[test]
    fn usage_change_maps_to_acp_usage_update() {
        let update = serde_json::to_value(usage_update(15, 200_000)).unwrap();
        assert_eq!(update["sessionUpdate"], "usage_update");
        assert_eq!(update["used"], 15);
        assert_eq!(update["size"], 200_000);
    }

    #[test]
    fn loaded_snapshot_exposes_persisted_usage() {
        let snapshot = json!({
            "usage": {
                "used": 15,
                "size": 200_000
            }
        });
        assert_eq!(snapshot_usage(&snapshot), Some((15, 200_000)));
    }

    #[test]
    fn terminal_tool_start_exposes_command_details() {
        let tool = tool_started(&json!({
            "call": {
                "tool_call_id": "call-1",
                "tool_name": "terminal",
                "raw_input": { "command": "cargo test --workspace" }
            }
        }));

        assert_eq!(tool.kind, ToolKind::Other);
        let json = serde_json::to_value(tool).unwrap();
        assert_eq!(json["rawInput"]["command"], "cargo test --workspace");
        assert_eq!(
            json["content"][0]["content"]["text"],
            "cargo test --workspace"
        );
    }

    #[test]
    fn terminal_tool_completion_exposes_output_details() {
        let update = tool_completed(&json!({
            "result": {
                "tool_call_id": "call-1",
                "tool_name": "terminal",
                "output": {
                    "status": "success",
                    "output": "test result: ok"
                }
            }
        }));

        let json = serde_json::to_value(update).unwrap();
        assert_eq!(json["status"], "completed");
        assert_eq!(json["rawOutput"]["output"], "test result: ok");
        assert_eq!(json["content"][0]["content"]["text"], "test result: ok");
    }

    #[test]
    fn file_edit_uses_generic_tool_card_with_raw_input() {
        let tool = tool_started(&json!({
            "call": {
                "tool_call_id": "call-2",
                "tool_name": "file_edit",
                "raw_input": { "patch": "*** Begin Patch\n*** End Patch" }
            }
        }));

        assert_eq!(tool.kind, ToolKind::Other);
        let json = serde_json::to_value(tool).unwrap();
        assert_eq!(json["rawInput"]["patch"], "*** Begin Patch\n*** End Patch");
    }
}
