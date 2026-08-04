use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use agent_client_protocol::schema::v2::{
    AgentCapabilities, AgentMessage, AgentThought, CancelSessionNotification, CloseSessionRequest,
    CloseSessionResponse, ConfigOptionUpdate, Content as ToolContent, ContentBlock, ContentChunk,
    DeleteSessionRequest, DeleteSessionResponse, Diff, EmbeddedResourceResource, IdleStateUpdate,
    Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptCapabilities, PromptEmbeddedContextCapabilities,
    PromptImageCapabilities, PromptRequest, PromptResponse, ReplayFrom, RequestPermissionOutcome,
    RequestPermissionRequest, ResourceLink, ResumeSessionRequest, ResumeSessionResponse,
    RunningStateUpdate, SessionCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionDeleteCapabilities, SessionId, SessionInfo,
    SessionInfoUpdate, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StateUpdate, StopReason, Terminal, TerminalExitStatus,
    TerminalOutput, TerminalOutputChunk, TerminalUpdate, TextContent, ToolCallContent, ToolCallId,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolKind, UpdateSessionNotification,
    UsageUpdate, UserMessage,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Error as AcpError, Responder,
    on_receive_notification, on_receive_request,
};
use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use dwo_context::{ContentBlock as DwoContentBlock, MessageContent};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::ipc;

#[derive(Clone)]
struct AcpRuntime {
    config_path: PathBuf,
    observers: Arc<Mutex<HashMap<String, Arc<SessionObserver>>>>,
}

struct SessionObserver {
    endpoint_id: String,
}

pub async fn run(config_path: PathBuf) -> Result<()> {
    run_with_io(config_path, tokio::io::stdin(), tokio::io::stdout()).await
}

struct EofReader<R> {
    inner: R,
    eof: CancellationToken,
}

impl<R> EofReader<R> {
    fn new(inner: R, eof: CancellationToken) -> Self {
        Self { inner, eof }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for EofReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled = buffer.filled().len();
        let had_capacity = buffer.remaining() > 0;
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        if had_capacity && matches!(result, Poll::Ready(Ok(()))) && buffer.filled().len() == filled
        {
            self.eof.cancel();
        }
        result
    }
}

pub(crate) async fn run_with_io<R, W>(config_path: PathBuf, stdin: R, stdout: W) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let eof = CancellationToken::new();
    let stdin = EofReader::new(stdin, eof.clone()).compat();
    let stdout = stdout.compat_write();
    let transport = ByteStreams::new(stdout, stdin);
    let runtime = AcpRuntime {
        config_path: config_path.clone(),
        observers: Arc::new(Mutex::new(HashMap::new())),
    };
    let new_config = config_path.clone();
    let list_config = config_path.clone();
    let resume_runtime = runtime.clone();
    let prompt_runtime = runtime.clone();
    let close_runtime = runtime.clone();
    let delete_runtime = runtime.clone();
    let set_runtime = runtime.clone();
    let cancel_runtime = runtime;

    let agent = Agent
        .v2()
        .on_receive_request(
            async move |request: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(
                        request.protocol_version,
                        Implementation::new("dwo", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(
                        AgentCapabilities::new().session(
                            SessionCapabilities::new()
                                .prompt(
                                    PromptCapabilities::new()
                                        .image(PromptImageCapabilities::new())
                                        .embedded_context(PromptEmbeddedContextCapabilities::new()),
                                )
                                .delete(SessionDeleteCapabilities::new()),
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
                if let Err(error) = validate_new_session(&request) {
                    return responder.respond_with_error(invalid_params(error));
                }
                match ipc::request(
                    &new_config,
                    "session.new",
                    json!({"cwd": request.cwd.into_inner(), "title": Value::Null}),
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
                match ipc::request(&list_config, "session.list", json!({"all": true})).await {
                    Ok(value) => {
                        let records: Vec<dwo_agent_service::SessionRecord> =
                            serde_json::from_value(value).unwrap_or_default();
                        let cwd = request.cwd;
                        let sessions = records
                            .into_iter()
                            .filter(|record| {
                                cwd.as_ref()
                                    .is_none_or(|cwd| record.info.cwd == AsRef::<Path>::as_ref(cwd))
                            })
                            .map(|record| {
                                SessionInfo::new(
                                    SessionId::new(record.info.id.as_str()),
                                    record.info.cwd,
                                )
                                .title(record.info.title)
                                .updated_at(timestamp_rfc3339(record.info.updated_at_ms))
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
            async move |request: ResumeSessionRequest,
                        responder: Responder<ResumeSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let runtime = resume_runtime.clone();
                cx.clone().spawn(async move {
                    let session_id = request.session_id.to_string();
                    match validate_resume(&runtime, &request).await {
                        Ok(()) => {}
                        Err(error) => {
                            responder.respond_with_error(invalid_params(error))?;
                            return Ok(());
                        }
                    }
                    let replay = request.replay_from.is_some();
                    match ensure_observer(&runtime, &session_id, &cx, replay).await {
                        Ok(_) => {
                            match session_config_options(&runtime.config_path, &session_id).await {
                                Ok(options) => responder
                                    .respond(ResumeSessionResponse::new().config_options(options)),
                                Err(error) => responder.respond_with_error(internal_error(error)),
                            }
                        }
                        Err(error) => responder.respond_with_error(internal_error(error)),
                    }?;
                    Ok(())
                })?;
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest,
                        responder: Responder<CloseSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                let runtime = close_runtime.clone();
                let session_id = request.session_id.to_string();
                match ipc::request(
                    &runtime.config_path,
                    "session.close",
                    json!({"session_id": session_id}),
                )
                .await
                {
                    Ok(_) => {
                        runtime.observers.lock().await.remove(&session_id);
                        responder.respond(CloseSessionResponse::new())
                    }
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: DeleteSessionRequest,
                        responder: Responder<DeleteSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                let runtime = delete_runtime.clone();
                let session_id = request.session_id.to_string();
                match ipc::request(
                    &runtime.config_path,
                    "session.delete",
                    json!({"session_id": session_id}),
                )
                .await
                {
                    Ok(_) => {
                        runtime.observers.lock().await.remove(&session_id);
                        responder.respond(DeleteSessionResponse::new())
                    }
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
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
                let Some(value) = request.value.as_id().map(ToString::to_string) else {
                    return responder
                        .respond_with_error(invalid_params("session config value must be an id"));
                };
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
            async move |notification: CancelSessionNotification, _cx: ConnectionTo<Client>| {
                let _ = ipc::request(
                    &cancel_runtime.config_path,
                    "session.cancel",
                    json!({"session_id": notification.session_id}),
                )
                .await;
                Ok(())
            },
            on_receive_notification!(),
        );
    connect_until_eof(agent, transport, eof)
        .await
        .map_err(|error| anyhow::anyhow!("ACP connection failed: {error}"))
}

async fn connect_until_eof<H, Run, Transport>(
    agent: agent_client_protocol::Builder<Agent, H, Run>,
    transport: Transport,
    eof: CancellationToken,
) -> agent_client_protocol::Result<()>
where
    H: agent_client_protocol::HandleDispatchFrom<Client> + 'static,
    Run: agent_client_protocol::RunWithConnectionTo<Client> + 'static,
    Transport: agent_client_protocol::ConnectTo<Agent> + 'static,
{
    agent
        .connect_with(transport, async move |_cx| {
            eof.cancelled().await;
            Ok(())
        })
        .await
}

async fn run_prompt(
    runtime: AcpRuntime,
    request: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) {
    let session_id = request.session_id.to_string();
    let prompt_blocks = request.prompt.clone();
    let content = match prompt_content(&request.prompt) {
        Ok(content) => content,
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
    match ipc::request(
        &runtime.config_path,
        "session.prompt",
        json!({
            "session_id": session_id,
            "endpoint_id": observer.endpoint_id,
            "message": content,
        }),
    )
    .await
    {
        Ok(value) => {
            let _ = responder.respond(PromptResponse::new());
            if let Some(message_id) = value.get("message_id").and_then(Value::as_str) {
                let prompt = serde_json::to_value(prompt_blocks)
                    .unwrap_or_else(|_| Value::Array(Vec::new()));
                send_user_message(&cx, &session_id, message_id, &prompt);
            }
        }
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
        }
    }
}

fn validate_new_session(request: &NewSessionRequest) -> Result<()> {
    anyhow::ensure!(
        request.additional_directories.is_empty(),
        "additionalDirectories are not supported"
    );
    anyhow::ensure!(
        request.mcp_servers.is_empty(),
        "MCP session setup is not supported"
    );
    Ok(())
}

async fn validate_resume(runtime: &AcpRuntime, request: &ResumeSessionRequest) -> Result<()> {
    anyhow::ensure!(
        request.additional_directories.is_empty(),
        "additionalDirectories are not supported"
    );
    anyhow::ensure!(
        request.mcp_servers.is_empty(),
        "MCP session setup is not supported"
    );
    if let Some(replay_from) = &request.replay_from {
        anyhow::ensure!(
            matches!(replay_from, ReplayFrom::Start(_)),
            "unsupported replay cursor"
        );
    }
    let value = ipc::request(
        &runtime.config_path,
        "session.snapshot",
        json!({"session_id": request.session_id}),
    )
    .await?;
    let snapshot: dwo_agent_service::SessionSnapshot = serde_json::from_value(value)?;
    let requested = normalize_path(AsRef::<Path>::as_ref(&request.cwd));
    let stored = normalize_path(&snapshot.record.info.cwd);
    anyhow::ensure!(
        requested == stored,
        "resume cwd does not match the session cwd"
    );
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    let observer = Arc::new(SessionObserver {
        endpoint_id: endpoint_id.clone(),
    });
    observers.insert(session_id.to_string(), observer.clone());

    let observer_runtime = runtime.clone();
    let observer_session_id = session_id.to_string();
    let observer_endpoint_id = endpoint_id;
    let observer_cx = cx.clone();
    if let Err(error) = cx.spawn(async move {
        while let Some(frame) = events.recv().await {
            handle_session_event(
                &observer_runtime,
                &observer_cx,
                &observer_session_id,
                &observer_endpoint_id,
                frame,
            );
        }
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
    frame: Value,
) {
    let Some(payload) = frame.get("params").and_then(|event| event.get("payload")) else {
        return;
    };
    let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "assistant_delta" => {
            if let (Some(message_id), Some(delta)) = (
                payload.get("message_id").and_then(Value::as_str),
                payload.get("delta").and_then(Value::as_str),
            ) {
                send_agent_chunk(cx, session_id, message_id, delta);
            }
        }
        "assistant_reasoning_delta" => {
            if let (Some(message_id), Some(delta)) = (
                payload.get("message_id").and_then(Value::as_str),
                payload.get("delta").and_then(Value::as_str),
            ) {
                send_thought_chunk(cx, session_id, message_id, delta);
            }
        }
        "user_prompt_submitted" => {
            if payload.get("origin").and_then(Value::as_str) == Some(endpoint_id) {
                return;
            }
            if let (Some(message_id), Some(content)) = (
                payload.get("message_id").and_then(Value::as_str),
                payload.get("content"),
            ) {
                send_user_message(cx, session_id, message_id, content);
            }
        }
        "assistant_completed" => send_assistant_completed(cx, session_id, payload),
        "turn_started" => send_state(
            cx,
            session_id,
            StateUpdate::Running(RunningStateUpdate::new()),
        ),
        "tool_started" => send_tool_started(cx, session_id, payload),
        "tool_completed" => send_tool_completed(cx, session_id, payload),
        "terminal_opened" => send_terminal_opened(cx, session_id, payload),
        "terminal_output" => send_terminal_output(cx, session_id, payload),
        "terminal_exited" => send_terminal_exited(cx, session_id, payload),
        "file_read" => send_file_read(cx, session_id, payload),
        "file_changed" => send_file_changed(cx, session_id, payload),
        "permission_requested" => {
            send_state(
                cx,
                session_id,
                StateUpdate::RequiresAction(
                    agent_client_protocol::schema::v2::RequiresActionStateUpdate::new(),
                ),
            );
            let config_path = runtime.config_path.clone();
            let cx = cx.clone();
            let session_id = session_id.to_string();
            let endpoint_id = endpoint_id.to_string();
            let payload = payload.clone();
            let _ = cx.clone().spawn(async move {
                if let Err(error) =
                    resolve_permission(&config_path, &cx, &session_id, &endpoint_id, &payload).await
                {
                    tracing::error!(
                        event = "acp.permission_failed",
                        error = %format!("{error:#}"),
                        "ACP permission resolution failed"
                    );
                }
                Ok(())
            });
        }
        "permission_resolved" => send_state(
            cx,
            session_id,
            StateUpdate::Running(RunningStateUpdate::new()),
        ),
        "turn_completed" => send_idle(cx, session_id, StopReason::EndTurn),
        "turn_cancelled" => send_idle(cx, session_id, StopReason::Cancelled),
        "turn_failed" => send_idle(cx, session_id, StopReason::Other("_error".to_string())),
        "config_changed" => {
            let config_path = runtime.config_path.clone();
            let cx = cx.clone();
            let session_id = session_id.to_string();
            let _ = cx.clone().spawn(async move {
                match session_config_options(&config_path, &session_id).await {
                    Ok(options) => {
                        send_update(
                            &cx,
                            &session_id,
                            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options)),
                        );
                    }
                    Err(error) => tracing::warn!(
                        event = "acp.config_refresh_failed",
                        error = %format!("{error:#}"),
                        "refresh ACP config options failed"
                    ),
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
    let tool_name = permission["tool_name"].as_str().unwrap_or("tool");
    let tool = ToolCallUpdate::new(tool_call_id)
        .title(tool_name.to_string())
        .status(ToolCallStatus::Pending);
    let response = cx
        .send_request_to(
            Client,
            RequestPermissionRequest::new(
                SessionId::new(session_id),
                format!("Allow {tool_name}?"),
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
            )
            .subject(Some(tool.into())),
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

fn send_update(cx: &ConnectionTo<Client>, session_id: &str, update: SessionUpdate) {
    let _ = cx.send_notification_to(
        Client,
        UpdateSessionNotification::new(SessionId::new(session_id), update),
    );
}

fn send_user_message(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    message_id: &str,
    content: &Value,
) {
    send_update(
        cx,
        session_id,
        SessionUpdate::UserMessage(
            UserMessage::new(message_id.to_string()).content(acp_content_blocks(content)),
        ),
    );
}

fn send_agent_chunk(cx: &ConnectionTo<Client>, session_id: &str, message_id: &str, text: &str) {
    send_update(
        cx,
        session_id,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
            message_id.to_string(),
        )),
    );
}

fn send_thought_chunk(cx: &ConnectionTo<Client>, session_id: &str, message_id: &str, text: &str) {
    send_update(
        cx,
        session_id,
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
            message_id.to_string(),
        )),
    );
}

fn send_assistant_completed(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let (reasoning, content) = assistant_completed_text(payload);
    if let (Some(message_id), Some(reasoning)) = (
        payload.get("thought_message_id").and_then(Value::as_str),
        reasoning,
    ) {
        send_update(
            cx,
            session_id,
            SessionUpdate::AgentThought(AgentThought::new(message_id.to_string()).content(vec![
                ContentBlock::Text(TextContent::new(reasoning.to_string())),
            ])),
        );
    }
    if let (Some(message_id), Some(content)) =
        (payload.get("message_id").and_then(Value::as_str), content)
    {
        send_update(
            cx,
            session_id,
            SessionUpdate::AgentMessage(AgentMessage::new(message_id.to_string()).content(vec![
                ContentBlock::Text(TextContent::new(content.to_string())),
            ])),
        );
    }
}

fn send_state(cx: &ConnectionTo<Client>, session_id: &str, state: StateUpdate) {
    send_update(cx, session_id, SessionUpdate::StateUpdate(state));
}

fn send_idle(cx: &ConnectionTo<Client>, session_id: &str, reason: StopReason) {
    send_state(
        cx,
        session_id,
        StateUpdate::Idle(IdleStateUpdate::new().stop_reason(reason)),
    );
}

fn send_session_info_update(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    title: &str,
    updated_at_ms: Option<u64>,
) {
    let update = session_info_update(title, updated_at_ms);
    send_update(cx, session_id, update);
}

fn session_info_update(title: &str, updated_at_ms: Option<u64>) -> SessionUpdate {
    let mut update = SessionInfoUpdate::new().title(title.to_string());
    if let Some(updated_at_ms) = updated_at_ms {
        update = update.updated_at(timestamp_rfc3339(updated_at_ms));
    }
    SessionUpdate::SessionInfoUpdate(update)
}

fn send_usage_update(cx: &ConnectionTo<Client>, session_id: &str, used: u64, size: u64) {
    send_update(cx, session_id, usage_update(used, size));
}

fn usage_update(used: u64, size: u64) -> SessionUpdate {
    SessionUpdate::UsageUpdate(UsageUpdate::new(used, size))
}

fn send_tool_started(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let tool = tool_started(payload);
    send_update(cx, session_id, SessionUpdate::ToolCallUpdate(tool));
}

fn send_tool_completed(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let result = &payload["result"];
    if result["tool_name"].as_str() == Some("terminal")
        && let Some(terminal_id) = result["output"]["terminal_id"].as_str()
    {
        let mut terminal = TerminalUpdate::new(terminal_id.to_string());
        if let Some(output) = result["output"]["output"].as_str() {
            terminal = terminal.output(TerminalOutput::new(
                base64::engine::general_purpose::STANDARD.encode(output.as_bytes()),
            ));
        }
        send_update(cx, session_id, SessionUpdate::TerminalUpdate(terminal));
    }
    let update = tool_completed(payload);
    send_update(cx, session_id, SessionUpdate::ToolCallUpdate(update));
}

fn send_terminal_opened(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let terminal_id = payload["terminal_id"].as_str().unwrap_or("terminal");
    let mut terminal = TerminalUpdate::new(terminal_id.to_string())
        .command(payload["command"].as_str().unwrap_or_default().to_string());
    if let Some(cwd) = payload["cwd"].as_str() {
        terminal = terminal.cwd(PathBuf::from(cwd));
    }
    send_update(cx, session_id, SessionUpdate::TerminalUpdate(terminal));
    if let Some(tool_call_id) = payload["tool_call_id"].as_str() {
        send_update(
            cx,
            session_id,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(tool_call_id.to_string()).content(
                vec![ToolCallContent::Terminal(Terminal::new(
                    terminal_id.to_string(),
                ))],
            )),
        );
    }
}

fn send_terminal_output(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let Some(terminal_id) = payload["terminal_id"].as_str() else {
        return;
    };
    let Some(data) = payload["data"].as_array() else {
        return;
    };
    let bytes = data
        .iter()
        .filter_map(Value::as_u64)
        .filter_map(|value| u8::try_from(value).ok())
        .collect::<Vec<_>>();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    send_update(
        cx,
        session_id,
        SessionUpdate::TerminalOutputChunk(TerminalOutputChunk::new(
            terminal_id.to_string(),
            encoded,
        )),
    );
}

fn send_terminal_exited(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let Some(terminal_id) = payload["terminal_id"].as_str() else {
        return;
    };
    let mut status = TerminalExitStatus::new();
    if let Some(code) = payload["exit_code"]
        .as_i64()
        .and_then(|code| u32::try_from(code).ok())
    {
        status = status.exit_code(code);
    }
    send_update(
        cx,
        session_id,
        SessionUpdate::TerminalUpdate(
            TerminalUpdate::new(terminal_id.to_string()).exit_status(status),
        ),
    );
}

fn send_file_read(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let (Some(tool_call_id), Some(path)) =
        (payload["tool_call_id"].as_str(), payload["path"].as_str())
    else {
        return;
    };
    send_update(
        cx,
        session_id,
        SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(tool_call_id.to_string())
                .kind(ToolKind::Read)
                .status(ToolCallStatus::Completed)
                .locations(vec![ToolCallLocation::new(PathBuf::from(path))]),
        ),
    );
}

fn send_file_changed(cx: &ConnectionTo<Client>, session_id: &str, payload: &Value) {
    let (Some(tool_call_id), Some(changes)) = (
        payload["tool_call_id"].as_str(),
        payload["changes"].as_array(),
    ) else {
        return;
    };
    let changes = changes
        .iter()
        .filter_map(|change| {
            let path = PathBuf::from(change["path"].as_str()?);
            let kind = change["kind"].as_str().unwrap_or("update");
            Some(match kind {
                "add" => agent_client_protocol::schema::v2::DiffChange::add(path),
                "delete" => agent_client_protocol::schema::v2::DiffChange::delete(path),
                "move" => {
                    let moved_to = PathBuf::from(change["movedTo"].as_str()?);
                    agent_client_protocol::schema::v2::DiffChange::move_file(path, moved_to)
                }
                _ => agent_client_protocol::schema::v2::DiffChange::modify(path),
            })
        })
        .collect::<Vec<_>>();
    let patch = payload["patch"].as_str().filter(|patch| !patch.is_empty());
    let diff = match patch {
        Some(patch) => Diff::patch(patch, changes),
        None => Diff::new(changes),
    };
    let update = ToolCallUpdate::new(tool_call_id.to_string())
        .kind(ToolKind::Edit)
        .status(ToolCallStatus::Completed)
        .content(vec![ToolCallContent::Diff(diff)]);
    send_update(cx, session_id, SessionUpdate::ToolCallUpdate(update));
}

fn tool_started(payload: &Value) -> ToolCallUpdate {
    let call = &payload["call"];
    let id: ToolCallId = call["tool_call_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
        .into();
    let name = call["tool_name"].as_str().unwrap_or("tool");
    let raw_input = call.get("raw_input").cloned().unwrap_or(Value::Null);
    let mut tool = ToolCallUpdate::new(id)
        .title(name.to_string())
        .kind(tool_kind(name))
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
    let mut update = ToolCallUpdate::new(id)
        .status(if failed {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        })
        .raw_output(output.clone());
    if let Some(text) = render_tool_output(&output) {
        update = update.content(text_content(text));
    }
    update
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "terminal" => ToolKind::Execute,
        "file_edit" => ToolKind::Edit,
        "read_file" => ToolKind::Read,
        _ => ToolKind::Other,
    }
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
    vec![ToolCallContent::Content(Box::new(ToolContent::new(
        ContentBlock::Text(TextContent::new(text)),
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
                if let Some(message_id) = payload.get("message_id").and_then(Value::as_str) {
                    send_user_message(cx, session_id, message_id, &payload["content"]);
                }
            }
            Some("assistant_delta") => {
                if let (Some(message_id), Some(delta)) = (
                    payload.get("message_id").and_then(Value::as_str),
                    payload.get("delta").and_then(Value::as_str),
                ) {
                    send_agent_chunk(cx, session_id, message_id, delta);
                }
            }
            Some("assistant_reasoning_delta") => {
                if let (Some(message_id), Some(delta)) = (
                    payload.get("message_id").and_then(Value::as_str),
                    payload.get("delta").and_then(Value::as_str),
                ) {
                    send_thought_chunk(cx, session_id, message_id, delta);
                }
            }
            Some("assistant_completed") => send_assistant_completed(cx, session_id, payload),
            Some("tool_started") => send_tool_started(cx, session_id, payload),
            Some("tool_completed") => send_tool_completed(cx, session_id, payload),
            Some("terminal_opened") => send_terminal_opened(cx, session_id, payload),
            Some("terminal_exited") => send_terminal_exited(cx, session_id, payload),
            Some("file_read") => send_file_read(cx, session_id, payload),
            Some("file_changed") => send_file_changed(cx, session_id, payload),
            _ => {}
        }
    }
    match snapshot.get("phase").and_then(Value::as_str) {
        Some("running" | "cancelling") => send_state(
            cx,
            session_id,
            StateUpdate::Running(RunningStateUpdate::new()),
        ),
        Some("waiting_permission") => send_state(
            cx,
            session_id,
            StateUpdate::RequiresAction(
                agent_client_protocol::schema::v2::RequiresActionStateUpdate::new(),
            ),
        ),
        _ => send_state(cx, session_id, StateUpdate::Idle(IdleStateUpdate::new())),
    }
}

fn assistant_completed_text(payload: &Value) -> (Option<&str>, Option<&str>) {
    let non_empty_text = |field| {
        payload
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    (non_empty_text("reasoning"), non_empty_text("content"))
}

fn snapshot_usage(snapshot: &Value) -> Option<(u64, u64)> {
    let usage = snapshot.get("usage")?;
    Some((usage.get("used")?.as_u64()?, usage.get("size")?.as_u64()?))
}

fn timestamp_rfc3339(timestamp_ms: u64) -> String {
    i64::try_from(timestamp_ms)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn acp_content_blocks(value: &Value) -> Vec<ContentBlock> {
    serde_json::from_value(value.clone()).unwrap_or_else(|_| {
        let text = content_text(value);
        (!text.is_empty())
            .then(|| ContentBlock::Text(TextContent::new(text)))
            .into_iter()
            .collect()
    })
}

fn prompt_content(prompt: &[ContentBlock]) -> Result<MessageContent> {
    let mut output = Vec::with_capacity(prompt.len());
    for block in prompt {
        let projected = match block {
            ContentBlock::Text(content) => DwoContentBlock::text(content.text.clone()),
            ContentBlock::ResourceLink(resource) => {
                DwoContentBlock::text(resource_link_text(resource))
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(resource) => {
                    DwoContentBlock::text(embedded_resource_text(
                        &resource.uri,
                        resource.mime_type.as_ref().map(AsRef::as_ref),
                        &resource.text,
                    ))
                }
                EmbeddedResourceResource::BlobResourceContents(_) => {
                    anyhow::bail!("binary embedded resources are not supported")
                }
                _ => anyhow::bail!("unsupported embedded resource type"),
            },
            ContentBlock::Image(image) => DwoContentBlock::Image {
                mime_type: image.mime_type.to_string(),
                data: image.data.clone(),
                uri: image.uri.clone(),
                annotations: None,
                meta: None,
            },
            ContentBlock::Audio(_) => anyhow::bail!("audio input is not supported"),
            _ => anyhow::bail!("unsupported ACP content block"),
        };
        if !matches!(&projected, DwoContentBlock::Text { text, .. } if text.is_empty()) {
            output.push(projected);
        }
    }
    Ok(MessageContent::blocks(output))
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

fn internal_error(error: impl std::fmt::Display) -> AcpError {
    AcpError::internal_error().data(error.to_string())
}

fn invalid_params(error: impl std::fmt::Display) -> AcpError {
    AcpError::invalid_params().data(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v2::{
        BlobResourceContents, EmbeddedResource, ImageContent, TextResourceContents,
    };

    #[tokio::test]
    async fn acp_exits_when_the_client_closes_stdin() {
        let (client_stdin, agent_stdin) = tokio::io::duplex(1024);
        let (agent_stdout, _client_stdout) = tokio::io::duplex(1024);
        let eof = CancellationToken::new();
        let transport = ByteStreams::new(
            agent_stdout.compat_write(),
            EofReader::new(agent_stdin, eof.clone()).compat(),
        );
        let agent = Agent.v2().with_spawned(|cx| async move {
            let _connection_held_by_observer = cx;
            std::future::pending().await
        });
        let task = tokio::spawn(connect_until_eof(agent, transport, eof));

        drop(client_stdin);

        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("ACP runner should stop after stdin EOF")
            .expect("ACP runner task should not panic")
            .expect("stdin EOF should be a clean ACP shutdown");
    }

    #[test]
    fn acp_prompt_projects_text_embedded_text_and_resource_links_in_order() {
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

        let content = prompt_content(&prompt).unwrap();
        assert_eq!(content.as_blocks().len(), 3);
        assert_eq!(content.as_blocks()[0], DwoContentBlock::text("explain"));
        assert!(matches!(
            &content.as_blocks()[1],
            DwoContentBlock::Text { text, .. } if text.contains("README.md")
        ));
        assert!(matches!(
            &content.as_blocks()[2],
            DwoContentBlock::Text { text, .. } if text.contains("fn main() {}")
        ));
    }

    #[test]
    fn acp_prompt_accepts_images_and_rejects_binary_embedded_resources() {
        let image = [ContentBlock::Image(ImageContent::new(
            "aGVsbG8=",
            "image/png",
        ))];
        let content = prompt_content(&image).unwrap();
        assert!(matches!(
            &content.as_blocks()[0],
            DwoContentBlock::Image {
                mime_type,
                data,
                ..
            } if mime_type == "image/png" && data == "aGVsbG8="
        ));

        let blob = [ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(BlobResourceContents::new(
                "JVBERi0=",
                "attachment://report.pdf",
            )),
        ))];
        assert_eq!(
            prompt_content(&blob).unwrap_err().to_string(),
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
                max_model_steps: 100,
            },
            models: vec![SessionModelOption {
                id: "deepseek-v4-flash".to_string(),
                reasoning: vec!["high".to_string(), "max".to_string()],
                default_reasoning: "high".to_string(),
            }],
        })
        .unwrap();
        let json = serde_json::to_value(options).unwrap();
        assert_eq!(json[0]["configId"], "model");
        assert_eq!(json[0]["type"], "select");
        assert_eq!(json[0]["currentValue"], "deepseek-v4-flash");
        assert_eq!(json[1]["configId"], "reasoning_mode");
        assert_eq!(json[1]["type"], "select");
        assert_eq!(json[1]["currentValue"], "max");
        assert_eq!(json[2]["configId"], "policy_mode");
        assert_eq!(json[2]["type"], "select");
        assert_eq!(json[2]["currentValue"], "full_access");
    }

    #[test]
    fn title_change_maps_to_acp_session_info_update() {
        let update =
            serde_json::to_value(session_info_update("Generated title", Some(42))).unwrap();
        assert_eq!(update["sessionUpdate"], "session_info_update");
        assert_eq!(update["title"], "Generated title");
        assert_eq!(update["updatedAt"], "1970-01-01T00:00:00.042Z");
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
    fn assistant_completed_replays_reasoning_and_answer() {
        let payload = json!({
            "kind": "assistant_completed",
            "content": "final answer",
            "reasoning": "reasoning summary"
        });
        assert_eq!(
            assistant_completed_text(&payload),
            (Some("reasoning summary"), Some("final answer"))
        );

        let empty = json!({"content": "", "reasoning": null});
        assert_eq!(assistant_completed_text(&empty), (None, None));
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

        let json = serde_json::to_value(tool).unwrap();
        assert_eq!(json["kind"], "execute");
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

        let json = serde_json::to_value(tool).unwrap();
        assert_eq!(json["kind"], "edit");
        assert_eq!(json["rawInput"]["patch"], "*** Begin Patch\n*** End Patch");
    }
}
