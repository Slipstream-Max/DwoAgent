use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use agent_client_protocol_schema::v2::{
    AgentCapabilities, AgentMessage, AgentThought, AvailableCommand, AvailableCommandsUpdate,
    CancelSessionNotification, CloseSessionRequest, CloseSessionResponse, ConfigOptionUpdate,
    Content as ToolContent, ContentBlock, ContentChunk, DeleteSessionRequest,
    DeleteSessionResponse, Diff, EmbeddedResourceResource, IdleStateUpdate, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptCapabilities, PromptEmbeddedContextCapabilities, PromptImageCapabilities, PromptRequest,
    PromptResponse, ReplayFrom, RequestPermissionOutcome, RequestPermissionRequest, ResourceLink,
    ResumeSessionRequest, ResumeSessionResponse, RunningStateUpdate, SessionCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionDeleteCapabilities, SessionId, SessionInfo, SessionInfoUpdate, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StateUpdate, StopReason,
    Terminal, TerminalExitStatus, TerminalOutput, TerminalOutputChunk, TerminalUpdate, TextContent,
    ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolKind,
    UpdateSessionNotification, UsageUpdate, UserMessage,
};
use agent_client_protocol_schema::{ProtocolVersion, v1, v2};
use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use clap::ValueEnum;
use dwo_context::{ContentBlock as DwoContentBlock, MessageContent};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::acp_stdio::{self, Connection as AcpConnection, Incoming, RpcError};
use super::ipc;
use super::ipc_schema::{self, SessionOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AcpProtocol {
    V1,
    V2,
}

#[derive(Clone)]
struct AcpRuntime {
    config_path: PathBuf,
    protocol: AcpProtocol,
    observers: Arc<Mutex<HashMap<String, Arc<SessionObserver>>>>,
    prompt_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<StopReason>>>>,
    pending_cancels: PendingCancels,
}

const SEND_NOW_GRACE_PERIOD: Duration = Duration::from_millis(150);

#[derive(Clone, Default)]
struct PendingCancels {
    sessions: Arc<StdMutex<HashMap<String, Arc<CancellationToken>>>>,
}

impl PendingCancels {
    fn schedule(&self, session_id: String) -> Option<Arc<CancellationToken>> {
        let mut sessions = self.sessions.lock().expect("pending cancels lock poisoned");
        if sessions.contains_key(&session_id) {
            return None;
        }
        let cancellation = Arc::new(CancellationToken::new());
        sessions.insert(session_id, cancellation.clone());
        Some(cancellation)
    }

    fn consume(&self, session_id: &str) -> bool {
        let Some(cancellation) = self
            .sessions
            .lock()
            .expect("pending cancels lock poisoned")
            .remove(session_id)
        else {
            return false;
        };
        cancellation.cancel();
        true
    }

    fn finish(&self, session_id: &str, cancellation: &Arc<CancellationToken>) -> bool {
        let mut sessions = self.sessions.lock().expect("pending cancels lock poisoned");
        if sessions
            .get(session_id)
            .is_some_and(|pending| Arc::ptr_eq(pending, cancellation))
        {
            sessions.remove(session_id);
            true
        } else {
            false
        }
    }
}

struct SessionObserver {
    endpoint_id: String,
}

pub async fn run(config_path: PathBuf, protocol: AcpProtocol) -> Result<()> {
    run_with_protocol_io(
        config_path,
        protocol,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

pub(crate) async fn run_with_io<R, W>(config_path: PathBuf, stdin: R, stdout: W) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    run_with_protocol_io(config_path, AcpProtocol::V2, stdin, stdout).await
}

pub(crate) async fn run_with_protocol_io<R, W>(
    config_path: PathBuf,
    protocol: AcpProtocol,
    stdin: R,
    stdout: W,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let runtime = AcpRuntime {
        config_path,
        protocol,
        observers: Arc::new(Mutex::new(HashMap::new())),
        prompt_waiters: Arc::new(Mutex::new(HashMap::new())),
        pending_cancels: PendingCancels::default(),
    };
    acp_stdio::serve(stdin, stdout, move |connection, incoming| {
        dispatch_rpc(runtime.clone(), connection, incoming)
    })
    .await
}

fn dispatch_rpc(
    runtime: AcpRuntime,
    connection: AcpConnection,
    incoming: Incoming,
) -> impl std::future::Future<Output = Result<Option<Value>, RpcError>> + Send + 'static {
    let deferred_cancel = if incoming.method == "session/cancel" {
        match parse_request::<v1::CancelNotification, CancelSessionNotification>(
            runtime.protocol,
            incoming.params.clone(),
        ) {
            Ok(notification) => {
                defer_cancel(&runtime, &connection, notification.session_id.to_string());
                Some(Ok(()))
            }
            Err(error) => Some(Err(error)),
        }
    } else {
        if incoming.method == "session/prompt"
            && let Ok(request) = parse_request::<v1::PromptRequest, PromptRequest>(
                runtime.protocol,
                incoming.params.clone(),
            )
        {
            runtime
                .pending_cancels
                .consume(&request.session_id.to_string());
        }
        None
    };

    async move {
        match deferred_cancel {
            Some(result) => result.map(|()| None),
            None => handle_rpc(runtime, connection, incoming).await,
        }
    }
}

fn defer_cancel(runtime: &AcpRuntime, connection: &AcpConnection, session_id: String) {
    let Some(cancellation) = runtime.pending_cancels.schedule(session_id.clone()) else {
        return;
    };
    let runtime = runtime.clone();
    let connection_closed = connection.closed_token();
    tokio::spawn(async move {
        if cancellation_due(&cancellation, &connection_closed, SEND_NOW_GRACE_PERIOD).await {
            if runtime.pending_cancels.finish(&session_id, &cancellation) {
                let _ = ipc::request(
                    &runtime.config_path,
                    "session.cancel",
                    json!({"session_id": session_id}),
                )
                .await;
            }
        } else {
            let _ = runtime.pending_cancels.finish(&session_id, &cancellation);
        }
    });
}

async fn cancellation_due(
    cancellation: &CancellationToken,
    connection_closed: &CancellationToken,
    delay: Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = cancellation.cancelled() => false,
        _ = connection_closed.cancelled() => false,
    }
}

async fn handle_rpc(
    runtime: AcpRuntime,
    connection: AcpConnection,
    incoming: Incoming,
) -> Result<Option<Value>, RpcError> {
    let method = incoming.method.as_str();
    let result = match method {
        "initialize" => {
            let protocol_version = match runtime.protocol {
                AcpProtocol::V1 => {
                    serde_json::from_value::<v1::InitializeRequest>(incoming.params)
                        .map_err(invalid_params)?;
                    ProtocolVersion::V2
                }
                AcpProtocol::V2 => {
                    serde_json::from_value::<InitializeRequest>(incoming.params)
                        .map_err(invalid_params)?
                        .protocol_version
                }
            };
            let response = InitializeResponse::new(
                protocol_version,
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
            );
            match runtime.protocol {
                AcpProtocol::V1 => {
                    let mut response =
                        v1::InitializeResponse::try_from(response).map_err(internal_error)?;
                    response.protocol_version = ProtocolVersion::V1;
                    serde_json::to_value(response).map_err(internal_error)?
                }
                AcpProtocol::V2 => serde_json::to_value(response).map_err(internal_error)?,
            }
        }
        "session/new" => {
            let request: NewSessionRequest =
                parse_request::<v1::NewSessionRequest, _>(runtime.protocol, incoming.params)?;
            validate_new_session(&request).map_err(invalid_params)?;
            let value = ipc::request(
                &runtime.config_path,
                "session.new",
                json!({"cwd": request.cwd.into_inner(), "title": Value::Null}),
            )
            .await
            .map_err(internal_error)?;
            let id = value["session_id"].as_str().unwrap_or_default();
            let options = session_config_options(&runtime.config_path, id)
                .await
                .map_err(internal_error)?;
            if let Some((used, size)) = snapshot_usage(&value) {
                send_usage_update(&connection, &runtime, id, used, size);
            }
            send_available_commands(&connection, &runtime, id);
            versioned_response::<v1::NewSessionResponse, _>(
                runtime.protocol,
                NewSessionResponse::new(SessionId::new(id)).config_options(options),
            )?
        }
        "session/list" => {
            let request: ListSessionsRequest =
                parse_request::<v1::ListSessionsRequest, _>(runtime.protocol, incoming.params)?;
            let sessions = if request.cursor.is_some() {
                Vec::new()
            } else {
                let value =
                    ipc::request(&runtime.config_path, "session.list", json!({"all": true}))
                        .await
                        .map_err(internal_error)?;
                let records: Vec<ipc_schema::SessionRecord> =
                    serde_json::from_value(value).unwrap_or_default();
                records
                    .into_iter()
                    .filter(|record| {
                        request
                            .cwd
                            .as_ref()
                            .is_none_or(|cwd| record.info.cwd == AsRef::<Path>::as_ref(cwd))
                    })
                    .map(|record| {
                        SessionInfo::new(SessionId::new(record.info.id.as_str()), record.info.cwd)
                            .title(record.info.title)
                            .updated_at(timestamp_rfc3339(record.info.updated_at_ms))
                    })
                    .collect()
            };
            versioned_response::<v1::ListSessionsResponse, _>(
                runtime.protocol,
                ListSessionsResponse::new(sessions),
            )?
        }
        "session/resume" => {
            let request: ResumeSessionRequest =
                parse_request::<v1::ResumeSessionRequest, _>(runtime.protocol, incoming.params)?;
            resume_session(&runtime, &connection, request).await?
        }
        "session/load" if runtime.protocol == AcpProtocol::V1 => {
            let request: v1::LoadSessionRequest =
                serde_json::from_value(incoming.params).map_err(invalid_params)?;
            let request = ResumeSessionRequest::try_from(request).map_err(invalid_params)?;
            let response = resume_session_v2(&runtime, &connection, request).await?;
            let response: v1::LoadSessionResponse =
                serde_json::from_value(serde_json::to_value(response).map_err(internal_error)?)
                    .map_err(internal_error)?;
            serde_json::to_value(response).map_err(internal_error)?
        }
        "session/close" => {
            let request: CloseSessionRequest =
                parse_request::<v1::CloseSessionRequest, _>(runtime.protocol, incoming.params)?;
            let session_id = request.session_id.to_string();
            ipc::request(
                &runtime.config_path,
                "session.close",
                json!({"session_id": session_id}),
            )
            .await
            .map_err(internal_error)?;
            runtime.observers.lock().await.remove(&session_id);
            versioned_response::<v1::CloseSessionResponse, _>(
                runtime.protocol,
                CloseSessionResponse::new(),
            )?
        }
        "session/delete" => {
            let request: DeleteSessionRequest =
                parse_request::<v1::DeleteSessionRequest, _>(runtime.protocol, incoming.params)?;
            let session_id = request.session_id.to_string();
            ipc::request(
                &runtime.config_path,
                "session.delete",
                json!({"session_id": session_id}),
            )
            .await
            .map_err(internal_error)?;
            runtime.observers.lock().await.remove(&session_id);
            versioned_response::<v1::DeleteSessionResponse, _>(
                runtime.protocol,
                DeleteSessionResponse::new(),
            )?
        }
        "session/prompt" => {
            let request: PromptRequest =
                parse_request::<v1::PromptRequest, _>(runtime.protocol, incoming.params)?;
            return run_prompt(runtime, request, connection).await.map(Some);
        }
        "session/set_config_option" => {
            let request: SetSessionConfigOptionRequest =
                parse_request::<v1::SetSessionConfigOptionRequest, _>(
                    runtime.protocol,
                    incoming.params,
                )?;
            let session_id = request.session_id.to_string();
            let config_id = request.config_id.to_string();
            let value = request
                .value
                .as_id()
                .map(ToString::to_string)
                .ok_or_else(|| invalid_params("session config value must be an id"))?;
            let changed = ipc::request(
                &runtime.config_path,
                "session.set_config_option",
                json!({
                    "session_id": session_id,
                    "config_id": config_id,
                    "value": value,
                }),
            )
            .await
            .map_err(internal_error)?;
            let options = session_config_options(&runtime.config_path, &session_id)
                .await
                .map_err(internal_error)?;
            let observed = runtime.observers.lock().await.contains_key(&session_id);
            if config_id == "model"
                && !observed
                && let Some((used, size)) = snapshot_usage(&changed)
            {
                send_usage_update(&connection, &runtime, &session_id, used, size);
            }
            versioned_response::<v1::SetSessionConfigOptionResponse, _>(
                runtime.protocol,
                SetSessionConfigOptionResponse::new(options),
            )?
        }
        _ => return Err(RpcError::method_not_found(method)),
    };
    Ok(Some(result))
}

async fn resume_session(
    runtime: &AcpRuntime,
    connection: &AcpConnection,
    request: ResumeSessionRequest,
) -> Result<Value, RpcError> {
    let response = resume_session_v2(runtime, connection, request).await?;
    versioned_response::<v1::ResumeSessionResponse, _>(runtime.protocol, response)
}

async fn resume_session_v2(
    runtime: &AcpRuntime,
    connection: &AcpConnection,
    request: ResumeSessionRequest,
) -> Result<ResumeSessionResponse, RpcError> {
    let session_id = request.session_id.to_string();
    validate_resume(runtime, &request)
        .await
        .map_err(invalid_params)?;
    let replay = request.replay_from.is_some();
    ensure_observer(runtime, &session_id, connection, replay)
        .await
        .map_err(internal_error)?;
    let options = session_config_options(&runtime.config_path, &session_id)
        .await
        .map_err(internal_error)?;
    send_available_commands(connection, runtime, &session_id);
    Ok(ResumeSessionResponse::new().config_options(options))
}

fn parse_request<V1, V2>(protocol: AcpProtocol, params: Value) -> Result<V2, RpcError>
where
    V1: DeserializeOwned,
    V2: DeserializeOwned + TryFrom<V1>,
    <V2 as TryFrom<V1>>::Error: std::fmt::Display,
{
    match protocol {
        AcpProtocol::V1 => {
            let request = serde_json::from_value::<V1>(params).map_err(invalid_params)?;
            V2::try_from(request).map_err(invalid_params)
        }
        AcpProtocol::V2 => serde_json::from_value(params).map_err(invalid_params),
    }
}

fn versioned_response<V1, V2>(protocol: AcpProtocol, response: V2) -> Result<Value, RpcError>
where
    V1: Serialize + TryFrom<V2>,
    V2: Serialize,
    <V1 as TryFrom<V2>>::Error: std::fmt::Display,
{
    match protocol {
        AcpProtocol::V1 => serde_json::to_value(V1::try_from(response).map_err(internal_error)?)
            .map_err(internal_error),
        AcpProtocol::V2 => serde_json::to_value(response).map_err(internal_error),
    }
}

async fn run_prompt(
    runtime: AcpRuntime,
    request: PromptRequest,
    connection: AcpConnection,
) -> Result<Value, RpcError> {
    let session_id = request.session_id.to_string();
    let prompt_blocks = request.prompt.clone();
    let content = prompt_content(&request.prompt).map_err(invalid_params)?;
    let command = parse_slash_command(&content).map_err(invalid_params)?;
    let observer = ensure_observer(&runtime, &session_id, &connection, false)
        .await
        .map_err(internal_error)?;
    let completion = if runtime.protocol == AcpProtocol::V1 {
        let (sender, receiver) = oneshot::channel();
        let mut waiters = runtime.prompt_waiters.lock().await;
        if waiters.contains_key(&session_id) {
            return Err(invalid_params(
                "session already has an active ACP v1 prompt",
            ));
        }
        waiters.insert(session_id.clone(), sender);
        Some(receiver)
    } else {
        None
    };
    let request = match command {
        Some(SlashCommand::Compact) => {
            ipc::request(
                &runtime.config_path,
                "session.compact",
                json!({
                    "session_id": session_id,
                    "endpoint_id": observer.endpoint_id,
                }),
            )
            .await
        }
        Some(SlashCommand::Resume) => {
            ipc::request(
                &runtime.config_path,
                "session.resume-turn",
                json!({
                    "session_id": session_id,
                    "endpoint_id": observer.endpoint_id,
                }),
            )
            .await
        }
        None => {
            ipc::request(
                &runtime.config_path,
                "session.prompt",
                json!({
                    "session_id": session_id,
                    "endpoint_id": observer.endpoint_id,
                    "message": content,
                }),
            )
            .await
        }
    };
    let value = match request {
        Ok(value) => value,
        Err(error) => {
            runtime.prompt_waiters.lock().await.remove(&session_id);
            return Err(internal_error(error));
        }
    };
    if let Some(message_id) = value.get("message_id").and_then(Value::as_str) {
        let prompt =
            serde_json::to_value(prompt_blocks).unwrap_or_else(|_| Value::Array(Vec::new()));
        send_user_message(&connection, &runtime, &session_id, message_id, &prompt);
    }
    let Some(completion) = completion else {
        return serde_json::to_value(PromptResponse::new()).map_err(internal_error);
    };
    if value.get("accepted").and_then(Value::as_bool) == Some(false) {
        runtime.prompt_waiters.lock().await.remove(&session_id);
        return serde_json::to_value(v1::PromptResponse::new(v1::StopReason::EndTurn))
            .map_err(internal_error);
    }
    let reason = tokio::select! {
        reason = completion => reason.map_err(|_| internal_error("prompt completion was dropped"))?,
        _ = connection.closed() => {
            runtime.prompt_waiters.lock().await.remove(&session_id);
            return Err(internal_error("ACP connection closed"));
        },
    };
    let reason = v1::StopReason::try_from(reason).map_err(internal_error)?;
    serde_json::to_value(v1::PromptResponse::new(reason)).map_err(internal_error)
}

async fn complete_prompt(runtime: &AcpRuntime, session_id: &str, reason: StopReason) {
    if let Some(waiter) = runtime.prompt_waiters.lock().await.remove(session_id) {
        let _ = waiter.send(reason);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashCommand {
    Compact,
    Resume,
}

fn parse_slash_command(content: &MessageContent) -> Result<Option<SlashCommand>> {
    let Some(text) = content.as_text() else {
        return Ok(None);
    };
    let mut parts = text.split_whitespace();
    let Some(name) = parts.next() else {
        return Ok(None);
    };
    match name {
        "/compact" => {
            anyhow::ensure!(parts.next().is_none(), "/compact does not accept input");
            Ok(Some(SlashCommand::Compact))
        }
        "/resume" => {
            anyhow::ensure!(parts.next().is_none(), "/resume does not accept input");
            Ok(Some(SlashCommand::Resume))
        }
        _ => Ok(None),
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
    let snapshot: ipc_schema::SessionSnapshot = serde_json::from_value(value)?;
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
    connection: &AcpConnection,
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
            replay_snapshot(
                connection,
                runtime,
                session_id,
                &json!({"snapshot": snapshot}),
            );
        }
        return Ok(observer);
    }

    let endpoint_id = format!("acp-{}", Uuid::new_v4());
    let (snapshot, mut events) =
        ipc::subscribe(&runtime.config_path, session_id, &endpoint_id).await?;
    if replay {
        replay_snapshot(connection, runtime, session_id, &snapshot);
    }
    let observer = Arc::new(SessionObserver {
        endpoint_id: endpoint_id.clone(),
    });
    observers.insert(session_id.to_string(), observer.clone());

    let observer_runtime = runtime.clone();
    let observer_session_id = session_id.to_string();
    let observer_endpoint_id = endpoint_id;
    let observer_connection = connection.clone();
    tokio::spawn(async move {
        while let Some(frame) = events.recv().await {
            handle_session_event(
                &observer_runtime,
                &observer_connection,
                &observer_session_id,
                &observer_endpoint_id,
                frame,
            )
            .await;
        }
        let mut observers = observer_runtime.observers.lock().await;
        if observers
            .get(&observer_session_id)
            .is_some_and(|current| current.endpoint_id == observer_endpoint_id)
        {
            observers.remove(&observer_session_id);
        }
    });
    Ok(observer)
}

async fn handle_session_event(
    runtime: &AcpRuntime,
    connection: &AcpConnection,
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
                send_agent_chunk(connection, runtime, session_id, message_id, delta);
            }
        }
        "assistant_reasoning_delta" => {
            if let (Some(message_id), Some(delta)) = (
                payload.get("message_id").and_then(Value::as_str),
                payload.get("delta").and_then(Value::as_str),
            ) {
                send_thought_chunk(connection, runtime, session_id, message_id, delta);
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
                send_user_message(connection, runtime, session_id, message_id, content);
            }
        }
        "assistant_completed" => send_assistant_completed(connection, runtime, session_id, payload),
        "turn_started" => send_state(
            connection,
            runtime,
            session_id,
            StateUpdate::Running(RunningStateUpdate::new()),
        ),
        "tool_started" => send_tool_started(connection, runtime, session_id, payload),
        "tool_completed" => send_tool_completed(connection, runtime, session_id, payload),
        "terminal_opened" => send_terminal_opened(connection, runtime, session_id, payload),
        "terminal_output" => send_terminal_output(connection, runtime, session_id, payload),
        "terminal_exited" => send_terminal_exited(connection, runtime, session_id, payload),
        "file_read" => send_file_read(connection, runtime, session_id, payload),
        "file_changed" => send_file_changed(connection, runtime, session_id, payload),
        "permission_requested" => {
            send_state(
                connection,
                runtime,
                session_id,
                StateUpdate::RequiresAction(
                    agent_client_protocol_schema::v2::RequiresActionStateUpdate::new(),
                ),
            );
            let config_path = runtime.config_path.clone();
            let protocol = runtime.protocol;
            let connection = connection.clone();
            let session_id = session_id.to_string();
            let endpoint_id = endpoint_id.to_string();
            let payload = payload.clone();
            tokio::spawn(async move {
                if let Err(error) = resolve_permission(
                    &config_path,
                    protocol,
                    &connection,
                    &session_id,
                    &endpoint_id,
                    &payload,
                )
                .await
                {
                    tracing::error!(
                        event = "acp.permission_failed",
                        error = %format!("{error:#}"),
                        "ACP permission resolution failed"
                    );
                }
            });
        }
        "permission_resolved" => send_state(
            connection,
            runtime,
            session_id,
            StateUpdate::Running(RunningStateUpdate::new()),
        ),
        "turn_completed" => {
            send_idle(connection, runtime, session_id, StopReason::EndTurn);
            complete_prompt(runtime, session_id, StopReason::EndTurn).await;
        }
        "turn_cancelled" => {
            send_idle(connection, runtime, session_id, StopReason::Cancelled);
            complete_prompt(runtime, session_id, StopReason::Cancelled).await;
        }
        "turn_failed" => {
            let reason = StopReason::Other("_error".to_string());
            send_idle(connection, runtime, session_id, reason.clone());
            complete_prompt(runtime, session_id, reason).await;
        }
        "config_changed" => {
            let config_path = runtime.config_path.clone();
            let connection = connection.clone();
            let runtime = runtime.clone();
            let session_id = session_id.to_string();
            tokio::spawn(async move {
                match session_config_options(&config_path, &session_id).await {
                    Ok(options) => {
                        send_update(
                            &connection,
                            &runtime,
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
            });
        }
        "usage_changed" => {
            if let (Some(used), Some(size)) = (
                payload.get("used").and_then(Value::as_u64),
                payload.get("size").and_then(Value::as_u64),
            ) {
                send_usage_update(connection, runtime, session_id, used, size);
            }
        }
        "title_changed" => {
            if let Some(title) = payload.get("title").and_then(Value::as_str) {
                send_session_info_update(
                    connection,
                    runtime,
                    session_id,
                    title,
                    payload.get("updated_at_ms").and_then(Value::as_u64),
                );
            }
        }
        _ => {}
    }
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
    let snapshot: SessionOptions = serde_json::from_value(value)?;
    build_session_config_options(snapshot)
}

fn build_session_config_options(snapshot: SessionOptions) -> Result<Vec<SessionConfigOption>> {
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
    let policy = snapshot.config.mode.as_str();

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
    protocol: AcpProtocol,
    connection: &AcpConnection,
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
    let request = RequestPermissionRequest::new(
        SessionId::new(session_id),
        format!("Allow {tool_name}?"),
        vec![
            PermissionOption::new("allow_once", "Allow Once", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                "reject_once",
                "Reject Once",
                PermissionOptionKind::RejectOnce,
            ),
        ],
    )
    .subject(Some(tool.into()));
    let response = match protocol {
        AcpProtocol::V1 => {
            let request = v1::RequestPermissionRequest::try_from(request)?;
            let value = connection
                .request("session/request_permission", request)
                .await?;
            let response: v1::RequestPermissionResponse = serde_json::from_value(value)?;
            v2::RequestPermissionResponse::try_from(response)?
        }
        AcpProtocol::V2 => {
            let value = connection
                .request("session/request_permission", request)
                .await?;
            serde_json::from_value(value)?
        }
    };
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

fn send_update(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    update: SessionUpdate,
) {
    match runtime.protocol {
        AcpProtocol::V1 => {
            let Ok(updates) = Vec::<v1::SessionUpdate>::try_from(update) else {
                return;
            };
            for update in updates {
                let _ = connection.notify(
                    "session/update",
                    v1::SessionNotification::new(v1::SessionId::new(session_id), update),
                );
            }
        }
        AcpProtocol::V2 => {
            let _ = connection.notify(
                "session/update",
                UpdateSessionNotification::new(SessionId::new(session_id), update),
            );
        }
    }
}

fn send_available_commands(connection: &AcpConnection, runtime: &AcpRuntime, session_id: &str) {
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(available_commands())),
    );
}

fn available_commands() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand::new("compact", "Compact the current session context"),
        AvailableCommand::new("resume", "Continue the current session when it is idle"),
    ]
}

fn send_user_message(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    message_id: &str,
    content: &Value,
) {
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::UserMessage(
            UserMessage::new(message_id.to_string()).content(acp_content_blocks(content)),
        ),
    );
}

fn send_agent_chunk(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    message_id: &str,
    text: &str,
) {
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
            message_id.to_string(),
        )),
    );
}

fn send_thought_chunk(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    message_id: &str,
    text: &str,
) {
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
            message_id.to_string(),
        )),
    );
}

fn send_assistant_completed(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    payload: &Value,
) {
    let (reasoning, content) = assistant_completed_text(payload);
    if let (Some(message_id), Some(reasoning)) = (
        payload.get("thought_message_id").and_then(Value::as_str),
        reasoning,
    ) {
        send_update(
            connection,
            runtime,
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
            connection,
            runtime,
            session_id,
            SessionUpdate::AgentMessage(AgentMessage::new(message_id.to_string()).content(vec![
                ContentBlock::Text(TextContent::new(content.to_string())),
            ])),
        );
    }
}

fn send_state(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    state: StateUpdate,
) {
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::StateUpdate(state),
    );
}

fn send_idle(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    reason: StopReason,
) {
    send_state(
        connection,
        runtime,
        session_id,
        StateUpdate::Idle(IdleStateUpdate::new().stop_reason(reason)),
    );
}

fn send_session_info_update(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    title: &str,
    updated_at_ms: Option<u64>,
) {
    let update = session_info_update(title, updated_at_ms);
    send_update(connection, runtime, session_id, update);
}

fn session_info_update(title: &str, updated_at_ms: Option<u64>) -> SessionUpdate {
    let mut update = SessionInfoUpdate::new().title(title.to_string());
    if let Some(updated_at_ms) = updated_at_ms {
        update = update.updated_at(timestamp_rfc3339(updated_at_ms));
    }
    SessionUpdate::SessionInfoUpdate(update)
}

fn send_usage_update(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    used: u64,
    size: u64,
) {
    send_update(connection, runtime, session_id, usage_update(used, size));
}

fn usage_update(used: u64, size: u64) -> SessionUpdate {
    SessionUpdate::UsageUpdate(UsageUpdate::new(used, size))
}

fn send_tool_started(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    payload: &Value,
) {
    let tool = tool_started(payload);
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::ToolCallUpdate(tool),
    );
}

fn send_tool_completed(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    payload: &Value,
) {
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
        send_update(
            connection,
            runtime,
            session_id,
            SessionUpdate::TerminalUpdate(terminal),
        );
    }
    let update = tool_completed(payload);
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::ToolCallUpdate(update),
    );
}

fn send_terminal_opened(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    payload: &Value,
) {
    let terminal_id = payload["terminal_id"].as_str().unwrap_or("terminal");
    let mut terminal = TerminalUpdate::new(terminal_id.to_string())
        .command(payload["command"].as_str().unwrap_or_default().to_string());
    if let Some(cwd) = payload["cwd"].as_str() {
        terminal = terminal.cwd(PathBuf::from(cwd));
    }
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::TerminalUpdate(terminal),
    );
    if let Some(tool_call_id) = payload["tool_call_id"].as_str() {
        send_update(
            connection,
            runtime,
            session_id,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(tool_call_id.to_string()).content(
                vec![ToolCallContent::Terminal(Terminal::new(
                    terminal_id.to_string(),
                ))],
            )),
        );
    }
}

fn send_terminal_output(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    payload: &Value,
) {
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
        connection,
        runtime,
        session_id,
        SessionUpdate::TerminalOutputChunk(TerminalOutputChunk::new(
            terminal_id.to_string(),
            encoded,
        )),
    );
}

fn send_terminal_exited(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    payload: &Value,
) {
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
        connection,
        runtime,
        session_id,
        SessionUpdate::TerminalUpdate(
            TerminalUpdate::new(terminal_id.to_string()).exit_status(status),
        ),
    );
}

fn send_file_read(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    payload: &Value,
) {
    let (Some(tool_call_id), Some(path)) =
        (payload["tool_call_id"].as_str(), payload["path"].as_str())
    else {
        return;
    };
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(tool_call_id.to_string())
                .kind(ToolKind::Read)
                .status(ToolCallStatus::Completed)
                .locations(vec![ToolCallLocation::new(PathBuf::from(path))]),
        ),
    );
}

fn send_file_changed(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    payload: &Value,
) {
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
                "add" => agent_client_protocol_schema::v2::DiffChange::add(path),
                "delete" => agent_client_protocol_schema::v2::DiffChange::delete(path),
                "move" => {
                    let moved_to = PathBuf::from(change["movedTo"].as_str()?);
                    agent_client_protocol_schema::v2::DiffChange::move_file(path, moved_to)
                }
                _ => agent_client_protocol_schema::v2::DiffChange::modify(path),
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
    send_update(
        connection,
        runtime,
        session_id,
        SessionUpdate::ToolCallUpdate(update),
    );
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

fn replay_snapshot(
    connection: &AcpConnection,
    runtime: &AcpRuntime,
    session_id: &str,
    value: &Value,
) {
    let Some(snapshot) = value.get("snapshot") else {
        return;
    };
    if let Some((used, size)) = snapshot_usage(snapshot) {
        send_usage_update(connection, runtime, session_id, used, size);
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
        send_session_info_update(connection, runtime, session_id, title, updated_at_ms);
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
                    send_user_message(
                        connection,
                        runtime,
                        session_id,
                        message_id,
                        &payload["content"],
                    );
                }
            }
            Some("assistant_delta") => {
                if let (Some(message_id), Some(delta)) = (
                    payload.get("message_id").and_then(Value::as_str),
                    payload.get("delta").and_then(Value::as_str),
                ) {
                    send_agent_chunk(connection, runtime, session_id, message_id, delta);
                }
            }
            Some("assistant_reasoning_delta") => {
                if let (Some(message_id), Some(delta)) = (
                    payload.get("message_id").and_then(Value::as_str),
                    payload.get("delta").and_then(Value::as_str),
                ) {
                    send_thought_chunk(connection, runtime, session_id, message_id, delta);
                }
            }
            Some("assistant_completed") => {
                send_assistant_completed(connection, runtime, session_id, payload)
            }
            Some("tool_started") => send_tool_started(connection, runtime, session_id, payload),
            Some("tool_completed") => send_tool_completed(connection, runtime, session_id, payload),
            Some("terminal_opened") => {
                send_terminal_opened(connection, runtime, session_id, payload)
            }
            Some("terminal_exited") => {
                send_terminal_exited(connection, runtime, session_id, payload)
            }
            Some("file_read") => send_file_read(connection, runtime, session_id, payload),
            Some("file_changed") => send_file_changed(connection, runtime, session_id, payload),
            _ => {}
        }
    }
    match snapshot.get("phase").and_then(Value::as_str) {
        Some("running" | "cancelling") => send_state(
            connection,
            runtime,
            session_id,
            StateUpdate::Running(RunningStateUpdate::new()),
        ),
        Some("waiting_permission") => send_state(
            connection,
            runtime,
            session_id,
            StateUpdate::RequiresAction(
                agent_client_protocol_schema::v2::RequiresActionStateUpdate::new(),
            ),
        ),
        _ => send_state(
            connection,
            runtime,
            session_id,
            StateUpdate::Idle(IdleStateUpdate::new()),
        ),
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

fn internal_error(error: impl std::fmt::Display) -> RpcError {
    RpcError::internal(error)
}

fn invalid_params(error: impl std::fmt::Display) -> RpcError {
    RpcError::invalid_params(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v2::{
        BlobResourceContents, EmbeddedResource, ImageContent, TextResourceContents,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn pending_cancel_is_consumed_only_by_the_same_session() {
        let pending = PendingCancels::default();
        let first = pending.schedule("session-a".to_string()).unwrap();

        assert!(!pending.consume("session-b"));
        assert!(!first.is_cancelled());
        assert!(pending.consume("session-a"));
        assert!(first.is_cancelled());
    }

    #[test]
    fn duplicate_cancel_does_not_schedule_another_forward() {
        let pending = PendingCancels::default();

        assert!(pending.schedule("session-a".to_string()).is_some());
        assert!(pending.schedule("session-a".to_string()).is_none());
    }

    #[test]
    fn completed_cancel_cannot_remove_a_newer_cancel() {
        let pending = PendingCancels::default();
        let first = pending.schedule("session-a".to_string()).unwrap();
        assert!(pending.consume("session-a"));
        let second = pending.schedule("session-a".to_string()).unwrap();

        assert!(!pending.finish("session-a", &first));
        assert!(pending.finish("session-a", &second));
    }

    #[tokio::test]
    async fn cancel_wait_expires_unless_consumed_or_connection_closes() {
        let connection_closed = CancellationToken::new();
        assert!(
            cancellation_due(
                &CancellationToken::new(),
                &connection_closed,
                Duration::from_millis(1),
            )
            .await
        );

        let consumed = CancellationToken::new();
        consumed.cancel();
        assert!(!cancellation_due(&consumed, &connection_closed, Duration::from_secs(1)).await);

        connection_closed.cancel();
        assert!(
            !cancellation_due(
                &CancellationToken::new(),
                &connection_closed,
                Duration::from_secs(1),
            )
            .await
        );
    }

    #[tokio::test]
    async fn stdio_initializes_with_explicit_v1_and_v2_schemas() {
        for (protocol, params, expected) in [
            (
                AcpProtocol::V1,
                json!({"protocolVersion":1, "clientCapabilities":{}}),
                1,
            ),
            (
                AcpProtocol::V2,
                json!({
                    "protocolVersion":2,
                    "info":{"name":"test-client", "version":"1"},
                    "capabilities":{}
                }),
                2,
            ),
        ] {
            let (mut client_input, agent_input) = tokio::io::duplex(16 * 1024);
            let (agent_output, client_output) = tokio::io::duplex(16 * 1024);
            let task = tokio::spawn(run_with_protocol_io(
                PathBuf::from("unused-profile.yaml"),
                protocol,
                agent_input,
                agent_output,
            ));
            let request = json!({
                "jsonrpc":"2.0",
                "id":7,
                "method":"initialize",
                "params":params,
            });
            client_input
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            client_input.flush().await.unwrap();

            let mut response = String::new();
            BufReader::new(client_output)
                .read_line(&mut response)
                .await
                .unwrap();
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["id"], 7);
            assert_eq!(response["result"]["protocolVersion"], expected);
            let info = match protocol {
                AcpProtocol::V1 => &response["result"]["agentInfo"],
                AcpProtocol::V2 => &response["result"]["info"],
            };
            assert_eq!(info["name"], "dwo");

            drop(client_input);
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
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
    fn acp_recognizes_session_commands_without_claiming_other_slash_prompts() {
        assert!(matches!(
            parse_slash_command(&MessageContent::text(" /compact ")).unwrap(),
            Some(SlashCommand::Compact)
        ));
        assert!(matches!(
            parse_slash_command(&MessageContent::text("/resume")).unwrap(),
            Some(SlashCommand::Resume)
        ));
        assert!(
            parse_slash_command(&MessageContent::text("/custom value"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            parse_slash_command(&MessageContent::text("/compact now"))
                .unwrap_err()
                .to_string(),
            "/compact does not accept input"
        );
        assert_eq!(
            parse_slash_command(&MessageContent::text("/resume now"))
                .unwrap_err()
                .to_string(),
            "/resume does not accept input"
        );
    }

    #[test]
    fn acp_advertises_session_commands_as_v2_slash_commands() {
        let update = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
            available_commands(),
        ));
        let json = serde_json::to_value(update).unwrap();
        assert_eq!(json["sessionUpdate"], "available_commands_update");
        assert_eq!(json["availableCommands"][0]["name"], "compact");
        assert_eq!(
            json["availableCommands"][0]["description"],
            "Compact the current session context"
        );
        assert_eq!(json["availableCommands"][1]["name"], "resume");
    }

    #[test]
    fn acp_exposes_model_reasoning_and_policy_options() {
        let options = build_session_config_options(SessionOptions {
            config: ipc_schema::SessionConfig {
                mode: ipc_schema::SessionMode::FullAccess,
                model: "deepseek-v4-flash".to_string(),
                reasoning: Some("max".to_string()),
            },
            models: vec![ipc_schema::SessionModelOption {
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
