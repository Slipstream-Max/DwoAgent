use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use agent_client_protocol::schema::v1;
use agent_client_protocol::schema::v2::{
    AgentCapabilities, AgentMessage, AgentThought, AvailableCommand, AvailableCommandInput,
    AvailableCommandsUpdate, CancelSessionNotification, CloseSessionRequest, CloseSessionResponse,
    ConfigOptionUpdate, Content as ToolContent, ContentBlock, ContentChunk, DeleteSessionRequest,
    DeleteSessionResponse, Diff, EmbeddedResourceResource, ForkSessionRequest, ForkSessionResponse,
    IdleStateUpdate, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptCapabilities, PromptEmbeddedContextCapabilities,
    PromptImageCapabilities, PromptRequest, PromptResponse, ReplayFrom, RequestPermissionOutcome,
    RequestPermissionRequest, ResourceLink, ResumeSessionRequest, ResumeSessionResponse,
    RunningStateUpdate, SessionCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectGroup, SessionConfigSelectOption, SessionConfigSelectOptions,
    SessionDeleteCapabilities, SessionForkCapabilities, SessionId, SessionInfo, SessionInfoUpdate,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StateUpdate,
    StopReason, Terminal, TerminalExitStatus, TerminalOutput, TerminalOutputChunk, TerminalUpdate,
    TextCommandInput, TextContent, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolKind, UpdateSessionNotification, UsageUpdate, UserMessage,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Error as AcpError, Responder,
    on_receive_notification, on_receive_request,
};
use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use clap::ValueEnum;
use dwo_context::{ContentBlock as DwoContentBlock, MessageContent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Mutex, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::slash_commands::{
    SessionCommand as SlashCommand, parse_session_command as parse_slash_command,
};

use super::ipc;
use super::ipc_schema::{self, SessionOptions};

#[path = "acp_v1.rs"]
mod frontend_v1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AcpProtocol {
    V1,
    V2,
}

#[derive(Clone)]
struct AcpRuntime {
    config_path: PathBuf,
    observers: Arc<Mutex<HashMap<String, Arc<SessionObserver>>>>,
    prompt_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<PromptCompletion>>>>,
    pending_cancels: PendingCancels,
}

type PromptCompletion = Result<StopReason, String>;

// Zed's ACP v1 Send now sends the replacement prompt only after it receives
// the cancelled response. On Windows ARM64 the observed cancel-to-prompt
// round trip is about 256ms, so leave enough margin for normal IPC jitter.
const SEND_NOW_GRACE_PERIOD: Duration = Duration::from_millis(500);

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

struct PreparedObserver {
    observer: Arc<SessionObserver>,
    replay: Option<Value>,
    events: Option<tokio::sync::mpsc::Receiver<Value>>,
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
    match protocol {
        AcpProtocol::V1 => frontend_v1::run_with_io(config_path, stdin, stdout).await,
        AcpProtocol::V2 => run_with_io(config_path, stdin, stdout).await,
    }
}

#[derive(Clone)]
struct AcpConnection {
    protocol: AcpProtocol,
    inner: ConnectionTo<Client>,
    delivered: Arc<StdMutex<DeliveredMessages>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification)]
#[notification(method = "session/update")]
#[serde(rename_all = "camelCase")]
struct RawSessionNotification {
    session_id: String,
    update: Value,
}

#[derive(Default)]
struct DeliveredMessages {
    agent: HashSet<String>,
    thought: HashSet<String>,
    manual_compaction_turns: HashSet<String>,
}

impl AcpConnection {
    fn new(protocol: AcpProtocol, inner: ConnectionTo<Client>) -> Self {
        Self {
            protocol,
            inner,
            delivered: Arc::new(StdMutex::new(DeliveredMessages::default())),
        }
    }

    fn mark_manual_compaction(&self, turn_id: &str) {
        self.delivered
            .lock()
            .expect("delivered messages lock poisoned")
            .manual_compaction_turns
            .insert(turn_id.to_string());
    }

    fn is_manual_compaction_turn(&self, turn_id: &str) -> bool {
        self.delivered
            .lock()
            .expect("delivered messages lock poisoned")
            .manual_compaction_turns
            .contains(turn_id)
    }

    fn mark_streamed(&self, message_id: &str, thought: bool) {
        if self.protocol != AcpProtocol::V1 {
            return;
        }
        let mut delivered = self
            .delivered
            .lock()
            .expect("delivered messages lock poisoned");
        delivered.select_mut(thought).insert(message_id.to_string());
    }

    fn should_send_completed(&self, message_id: &str, thought: bool) -> bool {
        if self.protocol != AcpProtocol::V1 {
            return true;
        }
        let mut delivered = self
            .delivered
            .lock()
            .expect("delivered messages lock poisoned");
        delivered.select_mut(thought).insert(message_id.to_string())
    }
}

impl DeliveredMessages {
    fn select_mut(&mut self, thought: bool) -> &mut HashSet<String> {
        if thought {
            &mut self.thought
        } else {
            &mut self.agent
        }
    }
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
        prompt_waiters: Arc::new(Mutex::new(HashMap::new())),
        pending_cancels: PendingCancels::default(),
    };
    let new_config = config_path.clone();
    let list_config = config_path.clone();
    let fork_runtime = runtime.clone();
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
                                .fork(SessionForkCapabilities::new())
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
                                let connection = AcpConnection::new(AcpProtocol::V2, cx.clone());
                                let result = responder.respond(
                                    NewSessionResponse::new(SessionId::new(id))
                                        .config_options(options),
                                );
                                if let Some((used, size)) = snapshot_usage(&value) {
                                    send_usage_update(&connection, id, used, size);
                                }
                                send_available_commands(&new_config, &connection, id).await;
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
            async move |request: ForkSessionRequest,
                        responder: Responder<ForkSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                if let Err(error) = validate_fork(&fork_runtime, &request).await {
                    return responder.respond_with_error(invalid_params(error));
                }
                match fork_acp_session(&fork_runtime, &request.session_id.to_string()).await {
                    Ok(response) => responder.respond(response),
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
                        let records: Vec<ipc_schema::SessionRecord> =
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
                let connection = AcpConnection::new(AcpProtocol::V2, cx.clone());
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
                    match prepare_observer(&runtime, &session_id, replay).await {
                        Ok(prepared) => {
                            match session_config_options(&runtime.config_path, &session_id).await {
                                Ok(options) => {
                                    let result = responder.respond(
                                        ResumeSessionResponse::new().config_options(options),
                                    );
                                    if result.is_ok() {
                                        activate_observer(
                                            &runtime,
                                            &session_id,
                                            &connection,
                                            prepared,
                                        )
                                        .await?;
                                        send_available_commands(
                                            &runtime.config_path,
                                            &connection,
                                            &session_id,
                                        )
                                        .await;
                                    }
                                    result
                                }
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
                let connection = AcpConnection::new(AcpProtocol::V2, cx.clone());
                cx.clone().spawn(async move {
                    run_prompt(runtime, request, responder, connection).await;
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
                let connection = AcpConnection::new(AcpProtocol::V2, cx);
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
                                    send_usage_update(&connection, &session_id, used, size);
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
                cancel_session_now(&cancel_runtime, &notification.session_id.to_string()).await;
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
    cx: AcpConnection,
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
    let command = match parse_slash_command(&content) {
        Ok(command) => command,
        Err(error) => {
            let _ = responder.respond_with_error(invalid_params(error));
            return;
        }
    };
    let observer = match prepare_observer(&runtime, &session_id, false).await {
        Ok(prepared) => match activate_observer(&runtime, &session_id, &cx, prepared).await {
            Ok(observer) => observer,
            Err(error) => {
                let _ = responder.respond_with_error(internal_error(error));
                return;
            }
        },
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    if command == Some(SlashCommand::Status) {
        let result = async {
            let snapshot = load_status_snapshot(&runtime, &session_id).await?;
            let text = crate::session_status::render_status(
                &snapshot,
                crate::session_status::SessionIdDisplay::Full,
            );
            ipc::request(
                &runtime.config_path,
                "session.notify",
                json!({
                    "session_id": session_id,
                    "endpoint_id": observer.endpoint_id,
                    "category": "status",
                    "level": "info",
                    "text": text,
                    "data": {},
                }),
            )
            .await?;
            anyhow::Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                let _ = responder.respond(PromptResponse::new());
            }
            Err(error) => {
                let _ = responder.respond_with_error(internal_error(error));
            }
        }
        return;
    }
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
        Some(SlashCommand::Fork) => {
            ipc::request(
                &runtime.config_path,
                "session.fork",
                json!({"session_id": session_id}),
            )
            .await
        }
        Some(SlashCommand::Status) => unreachable!("status is handled before submitting a prompt"),
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
    match request {
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
    let snapshot: ipc_schema::SessionSnapshot = serde_json::from_value(value)?;
    let requested = normalize_path(AsRef::<Path>::as_ref(&request.cwd));
    let stored = normalize_path(&snapshot.record.info.cwd);
    anyhow::ensure!(
        requested == stored,
        "resume cwd does not match the session cwd"
    );
    Ok(())
}

async fn validate_fork(runtime: &AcpRuntime, request: &ForkSessionRequest) -> Result<()> {
    anyhow::ensure!(
        request.additional_directories.is_empty(),
        "additionalDirectories are not supported"
    );
    anyhow::ensure!(
        request.mcp_servers.is_empty(),
        "MCP session setup is not supported"
    );
    let value = ipc::request(
        &runtime.config_path,
        "session.snapshot",
        json!({"session_id": request.session_id}),
    )
    .await?;
    let snapshot: ipc_schema::SessionSnapshot = serde_json::from_value(value)?;
    anyhow::ensure!(
        normalize_path(AsRef::<Path>::as_ref(&request.cwd))
            == normalize_path(&snapshot.record.info.cwd),
        "fork cwd does not match the session cwd"
    );
    Ok(())
}

async fn fork_acp_session(runtime: &AcpRuntime, source_id: &str) -> Result<ForkSessionResponse> {
    let value = ipc::request(
        &runtime.config_path,
        "session.fork",
        json!({"session_id": source_id}),
    )
    .await?;
    let id = value
        .get("forked_session_id")
        .and_then(Value::as_str)
        .context("session.fork response omitted forked_session_id")?;
    let options = session_config_options(&runtime.config_path, id).await?;
    Ok(ForkSessionResponse::new(SessionId::new(id)).config_options(options))
}

fn normalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn prepare_observer(
    runtime: &AcpRuntime,
    session_id: &str,
    replay: bool,
) -> Result<PreparedObserver> {
    let mut observers = runtime.observers.lock().await;
    if let Some(observer) = observers.get(session_id).cloned() {
        drop(observers);
        let replay = if replay {
            Some(json!({
                "snapshot": ipc::request(
                &runtime.config_path,
                "session.snapshot",
                json!({"session_id": session_id}),
            )
            .await?
            }))
        } else {
            None
        };
        return Ok(PreparedObserver {
            observer,
            replay,
            events: None,
        });
    }

    let endpoint_id = format!("acp-{}", Uuid::new_v4());
    let (snapshot, events) = ipc::subscribe(&runtime.config_path, session_id, &endpoint_id).await?;
    let observer = Arc::new(SessionObserver {
        endpoint_id: endpoint_id.clone(),
    });
    observers.insert(session_id.to_string(), observer.clone());

    Ok(PreparedObserver {
        observer,
        replay: replay.then_some(snapshot),
        events: Some(events),
    })
}

async fn defer_cancel_v1(runtime: &AcpRuntime, session_id: String) {
    let Some(cancellation) = runtime.pending_cancels.schedule(session_id.clone()) else {
        complete_prompt(runtime, &session_id, Ok(StopReason::Cancelled)).await;
        return;
    };
    // v1 clients wait for the active prompt response before sending Send Now's
    // replacement prompt, so complete that response before the grace window.
    complete_prompt(runtime, &session_id, Ok(StopReason::Cancelled)).await;
    spawn_deferred_cancel(runtime.clone(), session_id, cancellation);
}

fn spawn_deferred_cancel(
    runtime: AcpRuntime,
    session_id: String,
    cancellation: Arc<CancellationToken>,
) {
    let runtime = runtime.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(SEND_NOW_GRACE_PERIOD) => {
                if runtime.pending_cancels.finish(&session_id, &cancellation) {
                    cancel_session_now(&runtime, &session_id).await;
                }
            }
            _ = cancellation.cancelled() => {}
        }
    });
}

fn spawn_cancel_now(runtime: AcpRuntime, session_id: String) {
    tokio::spawn(async move {
        cancel_session_now(&runtime, &session_id).await;
    });
}

async fn cancel_session_now(runtime: &AcpRuntime, session_id: &str) {
    let _ = ipc::request(
        &runtime.config_path,
        "session.cancel",
        json!({"session_id": session_id}),
    )
    .await;
}

async fn activate_observer(
    runtime: &AcpRuntime,
    session_id: &str,
    cx: &AcpConnection,
    mut prepared: PreparedObserver,
) -> Result<Arc<SessionObserver>> {
    if let Some(snapshot) = prepared.replay.take() {
        replay_snapshot(cx, session_id, &snapshot);
    }
    let Some(mut events) = prepared.events.take() else {
        return Ok(prepared.observer);
    };

    let observer_runtime = runtime.clone();
    let observer_session_id = session_id.to_string();
    let observer_endpoint_id = prepared.observer.endpoint_id.clone();
    let observer_cx = cx.clone();
    if let Err(error) = cx.inner.spawn(async move {
        while let Some(frame) = events.recv().await {
            handle_session_event(
                &observer_runtime,
                &observer_cx,
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
        Ok(())
    }) {
        runtime.observers.lock().await.remove(session_id);
        return Err(error.into());
    }
    Ok(prepared.observer)
}

async fn handle_session_event(
    runtime: &AcpRuntime,
    cx: &AcpConnection,
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
        "assistant_interrupted" => send_assistant_interrupted(cx, session_id, payload),
        "notification" => send_notification(cx, session_id, payload),
        "turn_started" => send_state(
            cx,
            session_id,
            StateUpdate::Running(RunningStateUpdate::new()),
        ),
        "tool_started" => send_tool_started(cx, session_id, payload),
        "tool_updated" => send_tool_updated(cx, session_id, payload),
        "tool_completed" => send_tool_completed(cx, session_id, payload),
        "terminal_opened" => send_terminal_opened(cx, session_id, payload),
        "terminal_output" => send_terminal_output(cx, session_id, payload),
        "terminal_exited" => send_terminal_exited(cx, session_id, payload),
        "file_read" => send_file_read(cx, session_id, payload),
        "file_changed" => send_file_changed(cx, session_id, payload),
        "compaction_started"
        | "compaction_completed"
        | "compaction_failed"
        | "compaction_cancelled" => send_legacy_compaction_notification(cx, session_id, payload),
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
            let _ = cx.inner.clone().spawn(async move {
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
        "turn_completed" => {
            send_idle(cx, session_id, StopReason::EndTurn);
            complete_prompt(runtime, session_id, Ok(StopReason::EndTurn)).await;
        }
        "turn_cancelled" => {
            send_idle(cx, session_id, StopReason::Cancelled);
            complete_prompt(runtime, session_id, Ok(StopReason::Cancelled)).await;
        }
        "turn_failed" => {
            send_idle(cx, session_id, StopReason::Other("_error".to_string()));
            let error = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("turn failed")
                .to_string();
            complete_prompt(runtime, session_id, Err(error)).await;
        }
        "config_changed" => {
            let config_path = runtime.config_path.clone();
            let cx = cx.clone();
            let session_id = session_id.to_string();
            let _ = cx.inner.clone().spawn(async move {
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

async fn complete_prompt(runtime: &AcpRuntime, session_id: &str, completion: PromptCompletion) {
    if let Some(waiter) = runtime.prompt_waiters.lock().await.remove(session_id) {
        let _ = waiter.send(completion);
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

    // Group models by provider, preserving the catalog order.
    let mut provider_order: Vec<String> = Vec::new();
    let mut provider_options: Vec<Vec<SessionConfigSelectOption>> = Vec::new();
    for option in &snapshot.models {
        let index = match provider_order.iter().position(|id| id == &option.provider) {
            Some(index) => index,
            None => {
                provider_order.push(option.provider.clone());
                provider_options.push(Vec::new());
                provider_options.len() - 1
            }
        };
        provider_options[index].push(SessionConfigSelectOption::new(
            option.id.clone(),
            option.id.clone(),
        ));
    }
    let model_options: SessionConfigSelectOptions = if provider_order.len() > 1 {
        provider_order
            .into_iter()
            .zip(provider_options)
            .map(|(provider, options)| {
                SessionConfigSelectGroup::new(provider.clone(), provider, options)
            })
            .collect::<Vec<_>>()
            .into()
    } else {
        provider_options
            .into_iter()
            .next()
            .unwrap_or_default()
            .into()
    };

    Ok(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            snapshot.config.model.clone(),
            model_options,
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
    cx: &AcpConnection,
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
    let response = match cx.protocol {
        AcpProtocol::V1 => {
            let request = v1::RequestPermissionRequest::try_from(request)?;
            let response = cx
                .inner
                .send_request_to(Client, request)
                .block_task()
                .await?;
            agent_client_protocol::schema::v2::RequestPermissionResponse::try_from(response)?
        }
        AcpProtocol::V2 => {
            cx.inner
                .send_request_to(Client, request)
                .block_task()
                .await?
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

fn send_update(cx: &AcpConnection, session_id: &str, update: SessionUpdate) {
    match cx.protocol {
        AcpProtocol::V1 => {
            let Ok(updates) = Vec::<v1::SessionUpdate>::try_from(update) else {
                return;
            };
            for update in updates {
                let _ = cx.inner.send_notification_to(
                    Client,
                    v1::SessionNotification::new(v1::SessionId::new(session_id), update),
                );
            }
        }
        AcpProtocol::V2 => {
            let _ = cx.inner.send_notification_to(
                Client,
                UpdateSessionNotification::new(SessionId::new(session_id), update),
            );
        }
    }
}

async fn send_available_commands(config_path: &Path, cx: &AcpConnection, session_id: &str) {
    let options = match ipc::request(
        config_path,
        "session.prompt-directives",
        json!({"session_id": session_id}),
    )
    .await
    {
        Ok(value) => serde_json::from_value(value).unwrap_or_default(),
        Err(error) => {
            tracing::warn!(
                event = "acp.prompt_directives_failed",
                session_id,
                error = %format!("{error:#}"),
                "load ACP prompt directive completions failed"
            );
            ipc_schema::PromptDirectiveOptions::default()
        }
    };
    send_update(
        cx,
        session_id,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(available_commands(
            options,
        ))),
    );
}

fn available_commands(options: ipc_schema::PromptDirectiveOptions) -> Vec<AvailableCommand> {
    let mut commands = vec![
        AvailableCommand::new("compact", "Compact the current session context"),
        AvailableCommand::new("resume", "Continue the current session when it is idle"),
        AvailableCommand::new("fork", "Copy the current session into a new session"),
        AvailableCommand::new(
            "status",
            "Show the current session ID, model, and reasoning",
        ),
        AvailableCommand::new(
            "plan",
            "Pause and plan together before acting, without writing code",
        )
        .input(AvailableCommandInput::Text(TextCommandInput::new(
            "optional context",
        ))),
    ];
    // ACP has free-text command input but no schema for completing individual arguments.
    // Publishing the directive and name together lets clients complete `/skill review `.
    commands.extend(options.skills.into_iter().map(|skill| {
        let description = skill
            .description
            .filter(|description| !description.trim().is_empty())
            .unwrap_or_else(|| format!("Use the {} skill", skill.name));
        AvailableCommand::new(format!("skill {}", skill.name), description).input(
            AvailableCommandInput::Text(TextCommandInput::new("optional prompt")),
        )
    }));
    commands.extend(options.mcp_servers.into_iter().map(|server| {
        let description = server
            .description
            .filter(|description| !description.trim().is_empty())
            .unwrap_or_else(|| format!("Use the {} MCP server", server.name));
        AvailableCommand::new(format!("mcp {}", server.name), description).input(
            AvailableCommandInput::Text(TextCommandInput::new("optional prompt")),
        )
    }));
    commands
}

async fn load_status_snapshot(
    runtime: &AcpRuntime,
    session_id: &str,
) -> Result<dwo_agent_service::SessionSnapshot> {
    let value = ipc::request(
        &runtime.config_path,
        "session.snapshot",
        json!({"session_id": session_id}),
    )
    .await?;
    Ok(serde_json::from_value(value)?)
}

fn send_user_message(cx: &AcpConnection, session_id: &str, message_id: &str, content: &Value) {
    send_update(
        cx,
        session_id,
        SessionUpdate::UserMessage(
            UserMessage::new(message_id.to_string()).content(acp_content_blocks(content)),
        ),
    );
}

fn send_agent_chunk(cx: &AcpConnection, session_id: &str, message_id: &str, text: &str) {
    cx.mark_streamed(message_id, false);
    send_update(
        cx,
        session_id,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
            message_id.to_string(),
        )),
    );
}

fn send_thought_chunk(cx: &AcpConnection, session_id: &str, message_id: &str, text: &str) {
    cx.mark_streamed(message_id, true);
    send_update(
        cx,
        session_id,
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
            message_id.to_string(),
        )),
    );
}

fn send_assistant_completed(cx: &AcpConnection, session_id: &str, payload: &Value) {
    if payload
        .get("turn_id")
        .and_then(Value::as_str)
        .is_some_and(|turn_id| cx.is_manual_compaction_turn(turn_id))
    {
        return;
    }
    let (reasoning, content) = assistant_completed_text(payload);
    if let (Some(message_id), Some(reasoning)) = (
        payload.get("thought_message_id").and_then(Value::as_str),
        reasoning,
    ) && cx.should_send_completed(message_id, true)
    {
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
        && cx.should_send_completed(message_id, false)
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

fn send_assistant_interrupted(cx: &AcpConnection, session_id: &str, payload: &Value) {
    let meta = notification_meta(
        "interrupted_attempt",
        "warning",
        json!({
            "errorKind": payload.get("error_kind").cloned().unwrap_or(Value::Null),
        }),
    );
    if let (Some(message_id), Some(reasoning)) = (
        payload.get("thought_message_id").and_then(Value::as_str),
        payload
            .get("reasoning")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty()),
    ) {
        let send_content = cx.should_send_completed(message_id, true);
        if send_content || cx.protocol == AcpProtocol::V1 {
            let content = send_content
                .then(|| vec![ContentBlock::Text(TextContent::new(reasoning))])
                .unwrap_or_else(|| vec![ContentBlock::Text(TextContent::new("\u{200b}"))]);
            send_update(
                cx,
                session_id,
                SessionUpdate::AgentThought(
                    AgentThought::new(message_id.to_string())
                        .content(content)
                        .meta(meta.clone()),
                ),
            );
        }
    }
    if let (Some(message_id), Some(content)) = (
        payload.get("message_id").and_then(Value::as_str),
        payload
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty()),
    ) {
        let send_content = cx.should_send_completed(message_id, false);
        if send_content || cx.protocol == AcpProtocol::V1 {
            let content = send_content
                .then(|| vec![ContentBlock::Text(TextContent::new(content))])
                .unwrap_or_else(|| vec![ContentBlock::Text(TextContent::new("\u{200b}"))]);
            send_update(
                cx,
                session_id,
                SessionUpdate::AgentMessage(
                    AgentMessage::new(message_id.to_string())
                        .content(content)
                        .meta(meta),
                ),
            );
        }
    }
}

fn send_notification(cx: &AcpConnection, session_id: &str, payload: &Value) {
    let Some(message_id) = payload.get("message_id").and_then(Value::as_str) else {
        return;
    };
    let Some(text) = payload.get("text").and_then(Value::as_str) else {
        return;
    };
    let category = payload
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("notification");
    let level = payload
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("info");
    let data = payload.get("data").cloned().unwrap_or_else(|| json!({}));
    if category == "compaction_started"
        && data.get("trigger").and_then(Value::as_str) == Some("manual")
        && let Some(turn_id) = payload.get("turn_id").and_then(Value::as_str)
    {
        cx.mark_manual_compaction(turn_id);
    }
    send_update(
        cx,
        session_id,
        SessionUpdate::AgentMessage(
            AgentMessage::new(message_id.to_string())
                .content(vec![ContentBlock::Text(TextContent::new(text))])
                .meta(notification_meta(category, level, data)),
        ),
    );
}

fn notification_meta(category: &str, level: &str, data: Value) -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([(
        "dwo".to_string(),
        json!({
            "kind": "system_notification",
            "category": category,
            "level": level,
            "data": data,
        }),
    )])
}

fn send_state(cx: &AcpConnection, session_id: &str, state: StateUpdate) {
    send_update(cx, session_id, SessionUpdate::StateUpdate(state));
}

fn send_idle(cx: &AcpConnection, session_id: &str, reason: StopReason) {
    send_state(
        cx,
        session_id,
        StateUpdate::Idle(IdleStateUpdate::new().stop_reason(reason)),
    );
}

fn send_session_info_update(
    cx: &AcpConnection,
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

fn send_usage_update(cx: &AcpConnection, session_id: &str, used: u64, size: u64) {
    send_update(cx, session_id, usage_update(used, size));
}

fn usage_update(used: u64, size: u64) -> SessionUpdate {
    SessionUpdate::UsageUpdate(UsageUpdate::new(used, size))
}

fn send_legacy_compaction_notification(cx: &AcpConnection, session_id: &str, payload: &Value) {
    let Some(compaction_id) = payload.get("compaction_id").and_then(Value::as_str) else {
        return;
    };
    let (category, level, text) = match payload.get("kind").and_then(Value::as_str) {
        Some("compaction_started") => ("compaction_started", "info", "Compacting context..."),
        Some("compaction_completed") => ("compaction_completed", "success", "Context compacted."),
        Some("compaction_failed") => ("compaction_failed", "error", "Context compaction failed."),
        Some("compaction_cancelled") => (
            "compaction_cancelled",
            "warning",
            "Context compaction cancelled.",
        ),
        _ => return,
    };
    send_notification(
        cx,
        session_id,
        &json!({
            "message_id": format!("notification-{compaction_id}-{category}"),
            "turn_id": payload.get("turn_id").cloned().unwrap_or(Value::Null),
            "category": category,
            "level": level,
            "text": text,
            "data": {
                "compactionId": compaction_id,
                "trigger": payload.get("trigger").cloned().unwrap_or(Value::Null),
                "summary": payload.get("summary").cloned().unwrap_or(Value::Null),
                "error": payload.get("error").cloned().unwrap_or(Value::Null),
            },
        }),
    );
}

fn send_tool_started(cx: &AcpConnection, session_id: &str, payload: &Value) {
    let tool = tool_started(payload);
    match cx.protocol {
        AcpProtocol::V1 => {
            let Ok(update) = v1::ToolCallUpdate::try_from(tool) else {
                return;
            };
            let Ok(tool) = v1::ToolCall::try_from(update) else {
                return;
            };
            let _ = cx.inner.send_notification_to(
                Client,
                v1::SessionNotification::new(
                    v1::SessionId::new(session_id),
                    v1::SessionUpdate::ToolCall(tool),
                ),
            );
        }
        AcpProtocol::V2 => {
            send_update(cx, session_id, SessionUpdate::ToolCallUpdate(tool));
        }
    }
}

fn send_tool_updated(cx: &AcpConnection, session_id: &str, payload: &Value) {
    send_update(
        cx,
        session_id,
        SessionUpdate::ToolCallUpdate(tool_started(payload)),
    );
}

fn send_tool_completed(cx: &AcpConnection, session_id: &str, payload: &Value) {
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

fn send_terminal_opened(cx: &AcpConnection, session_id: &str, payload: &Value) {
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

fn send_terminal_output(cx: &AcpConnection, session_id: &str, payload: &Value) {
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

fn send_terminal_exited(cx: &AcpConnection, session_id: &str, payload: &Value) {
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

fn send_file_read(cx: &AcpConnection, session_id: &str, payload: &Value) {
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

fn send_file_changed(cx: &AcpConnection, session_id: &str, payload: &Value) {
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
        .kind(tool_kind())
        .status(acp_tool_status(call.get("status").and_then(Value::as_str)))
        .raw_input(raw_input.clone());
    if let Some(text) = render_tool_input(name, &raw_input) {
        tool = tool.content(text_content(text));
    }
    tool
}

fn acp_tool_status(status: Option<&str>) -> ToolCallStatus {
    match status {
        Some("pending") => ToolCallStatus::Pending,
        Some("completed") => ToolCallStatus::Completed,
        Some("failed" | "cancelled" | "canceled") => ToolCallStatus::Failed,
        _ => ToolCallStatus::InProgress,
    }
}

fn tool_completed(payload: &Value) -> ToolCallUpdate {
    let result = &payload["result"];
    let id: ToolCallId = result["tool_call_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
        .into();
    let failed = result["output"]["status"].as_str().is_some_and(|status| {
        matches!(
            status,
            "error" | "failed" | "cancelled" | "canceled" | "blocked_by_policy"
        )
    });
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
    vec![ToolCallContent::Content(Box::new(ToolContent::new(
        ContentBlock::Text(TextContent::new(text)),
    )))]
}

fn replay_snapshot(cx: &AcpConnection, session_id: &str, value: &Value) {
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
    if let Some(transcript) = snapshot.get("transcript").and_then(Value::as_array) {
        for event in transcript {
            let Some(payload) = event.get("payload") else {
                continue;
            };
            let kind = payload.get("kind").and_then(Value::as_str);
            match kind {
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
                Some("assistant_interrupted") => {
                    send_assistant_interrupted(cx, session_id, payload)
                }
                Some("notification") => send_notification(cx, session_id, payload),
                Some("tool_started") => send_tool_started(cx, session_id, payload),
                Some("tool_updated") => send_tool_updated(cx, session_id, payload),
                Some("tool_completed") => send_tool_completed(cx, session_id, payload),
                Some("terminal_opened") => send_terminal_opened(cx, session_id, payload),
                Some("terminal_exited") => send_terminal_exited(cx, session_id, payload),
                Some("file_read") => send_file_read(cx, session_id, payload),
                Some("file_changed") => send_file_changed(cx, session_id, payload),
                Some(
                    "compaction_started"
                    | "compaction_completed"
                    | "compaction_failed"
                    | "compaction_cancelled",
                ) => send_legacy_compaction_notification(cx, session_id, payload),
                _ => {}
            }
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
    fn completed_cancel_cannot_remove_a_newer_cancel() {
        let pending = PendingCancels::default();
        let first = pending.schedule("session-a".to_string()).unwrap();
        assert!(pending.consume("session-a"));
        let second = pending.schedule("session-a".to_string()).unwrap();

        assert!(!pending.finish("session-a", &first));
        assert!(pending.finish("session-a", &second));
    }

    #[test]
    fn v1_prompt_failure_preserves_the_host_error() {
        let error =
            frontend_v1::v1_stop_reason(Err("provider request failed".to_string())).unwrap_err();

        assert_eq!(error.to_string(), "provider request failed");
    }

    #[tokio::test]
    async fn stdio_initializes_with_explicit_v1_and_v2_agents() {
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
            let fork = match protocol {
                AcpProtocol::V1 => {
                    &response["result"]["agentCapabilities"]["sessionCapabilities"]["fork"]
                }
                AcpProtocol::V2 => &response["result"]["capabilities"]["session"]["fork"],
            };
            assert!(fork.is_object());

            drop(client_input);
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

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
    fn acp_recognizes_session_commands_without_claiming_other_slash_prompts() {
        assert!(matches!(
            parse_slash_command(&MessageContent::text(" /compact ")).unwrap(),
            Some(SlashCommand::Compact)
        ));
        assert!(matches!(
            parse_slash_command(&MessageContent::text("/resume")).unwrap(),
            Some(SlashCommand::Resume)
        ));
        assert!(matches!(
            parse_slash_command(&MessageContent::text("/fork")).unwrap(),
            Some(SlashCommand::Fork)
        ));
        assert!(matches!(
            parse_slash_command(&MessageContent::text(" /status ")).unwrap(),
            Some(SlashCommand::Status)
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
        assert_eq!(
            parse_slash_command(&MessageContent::text("/fork now"))
                .unwrap_err()
                .to_string(),
            "/fork does not accept input"
        );
        assert_eq!(
            parse_slash_command(&MessageContent::text("/status now"))
                .unwrap_err()
                .to_string(),
            "/status does not accept input"
        );
    }

    #[test]
    fn acp_advertises_session_and_prompt_directive_completions() {
        let update = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
            available_commands(ipc_schema::PromptDirectiveOptions {
                skills: vec![ipc_schema::PromptDirectiveOption {
                    name: "review".to_string(),
                    description: Some("Review changes".to_string()),
                }],
                mcp_servers: vec![ipc_schema::PromptDirectiveOption {
                    name: "github".to_string(),
                    description: None,
                }],
            }),
        ));
        let json = serde_json::to_value(update).unwrap();
        assert_eq!(json["sessionUpdate"], "available_commands_update");
        assert_eq!(json["availableCommands"][0]["name"], "compact");
        assert_eq!(
            json["availableCommands"][0]["description"],
            "Compact the current session context"
        );
        assert_eq!(json["availableCommands"][1]["name"], "resume");
        assert_eq!(json["availableCommands"][2]["name"], "fork");
        assert_eq!(json["availableCommands"][3]["name"], "status");
        assert_eq!(json["availableCommands"][4]["name"], "plan");
        assert_eq!(
            json["availableCommands"][4]["input"]["hint"],
            "optional context"
        );
        assert_eq!(json["availableCommands"][5]["name"], "skill review");
        assert_eq!(
            json["availableCommands"][5]["input"]["hint"],
            "optional prompt"
        );
        assert_eq!(json["availableCommands"][6]["name"], "mcp github");
    }

    #[test]
    fn acp_exposes_model_reasoning_and_policy_options() {
        let options = build_session_config_options(SessionOptions {
            config: ipc_schema::SessionConfig {
                mode: ipc_schema::SessionMode::FullAccess,
                model: "deepseek-v4-flash".to_string(),
                reasoning: Some("Max".to_string()),
            },
            models: vec![ipc_schema::SessionModelOption {
                id: "deepseek-v4-flash".to_string(),
                provider: "deepseek".to_string(),
                reasoning: vec!["Low".to_string(), "High".to_string(), "Max".to_string()],
                default_reasoning: "High".to_string(),
            }],
        })
        .unwrap();
        let json = serde_json::to_value(options).unwrap();
        assert_eq!(json[0]["configId"], "model");
        assert_eq!(json[0]["type"], "select");
        assert_eq!(json[0]["currentValue"], "deepseek-v4-flash");
        assert_eq!(json[1]["configId"], "reasoning_mode");
        assert_eq!(json[1]["type"], "select");
        assert_eq!(json[1]["currentValue"], "Max");
        assert_eq!(json[1]["options"][0]["value"], "Low");
        assert_eq!(json[1]["options"][1]["value"], "High");
        assert_eq!(json[1]["options"][2]["value"], "Max");
        assert_eq!(json[2]["configId"], "policy_mode");
        assert_eq!(json[2]["type"], "select");
        assert_eq!(json[2]["currentValue"], "full_access");
        assert_eq!(json[0]["options"][0]["value"], "deepseek-v4-flash");
    }

    #[test]
    fn acp_groups_models_by_provider() {
        let options = build_session_config_options(SessionOptions {
            config: ipc_schema::SessionConfig {
                mode: ipc_schema::SessionMode::FullAccess,
                model: "qwen3.8-max".to_string(),
                reasoning: Some("high".to_string()),
            },
            models: vec![
                ipc_schema::SessionModelOption {
                    id: "deepseek-v4-flash".to_string(),
                    provider: "deepseek".to_string(),
                    reasoning: vec!["high".to_string(), "max".to_string()],
                    default_reasoning: "high".to_string(),
                },
                ipc_schema::SessionModelOption {
                    id: "qwen3.8-max".to_string(),
                    provider: "tokenrhythm".to_string(),
                    reasoning: vec!["high".to_string()],
                    default_reasoning: "high".to_string(),
                },
            ],
        })
        .unwrap();
        let json = serde_json::to_value(options).unwrap();
        assert_eq!(json[0]["configId"], "model");
        assert_eq!(json[0]["currentValue"], "qwen3.8-max");
        let groups = &json[0]["options"];
        assert_eq!(groups[0]["groupId"], "deepseek");
        assert_eq!(groups[0]["name"], "deepseek");
        assert_eq!(groups[0]["options"][0]["value"], "deepseek-v4-flash");
        assert_eq!(groups[1]["groupId"], "tokenrhythm");
        assert_eq!(groups[1]["name"], "tokenrhythm");
        assert_eq!(groups[1]["options"][0]["value"], "qwen3.8-max");
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
    fn notifications_use_agent_message_metadata() {
        let update = SessionUpdate::AgentMessage(
            AgentMessage::new("notification-1")
                .content(vec![ContentBlock::Text(TextContent::new(
                    "Context compacted.",
                ))])
                .meta(notification_meta(
                    "compaction_completed",
                    "success",
                    json!({"compactionId": "cmp_001"}),
                )),
        );
        let value = serde_json::to_value(update).unwrap();
        assert_eq!(value["sessionUpdate"], "agent_message");
        assert_eq!(value["messageId"], "notification-1");
        assert_eq!(value["content"][0]["text"], "Context compacted.");
        assert_eq!(value["_meta"]["dwo"]["kind"], "system_notification");
        assert_eq!(value["_meta"]["dwo"]["category"], "compaction_completed");
        assert_eq!(value["_meta"]["dwo"]["level"], "success");
        assert_eq!(value["_meta"]["dwo"]["data"]["compactionId"], "cmp_001");
    }

    #[test]
    fn notification_metadata_survives_v1_conversion() {
        let update = SessionUpdate::AgentMessage(
            AgentMessage::new("notification-1")
                .content(vec![ContentBlock::Text(TextContent::new("Retrying."))])
                .meta(notification_meta(
                    "model_retrying",
                    "warning",
                    json!({"retry": 1}),
                )),
        );
        let converted = Vec::<v1::SessionUpdate>::try_from(update).unwrap();
        let value = serde_json::to_value(&converted[0]).unwrap();
        assert_eq!(value["sessionUpdate"], "agent_message_chunk");
        assert_eq!(value["_meta"]["dwo"]["category"], "model_retrying");
        assert_eq!(value["_meta"]["dwo"]["data"]["retry"], 1);
    }

    #[test]
    fn interrupted_metadata_survives_an_empty_v1_terminal_chunk() {
        let update = SessionUpdate::AgentMessage(
            AgentMessage::new("partial-1")
                .content(vec![ContentBlock::Text(TextContent::new("\u{200b}"))])
                .meta(notification_meta(
                    "interrupted_attempt",
                    "warning",
                    json!({"errorKind": "stream_interrupted"}),
                )),
        );
        let converted = Vec::<v1::SessionUpdate>::try_from(update).unwrap();
        let value = serde_json::to_value(&converted[0]).unwrap();
        assert_eq!(value["sessionUpdate"], "agent_message_chunk");
        assert_eq!(value["content"]["text"], "\u{200b}");
        assert_eq!(value["_meta"]["dwo"]["category"], "interrupted_attempt");
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
        assert_eq!(json["kind"], "other");
        assert_eq!(json["rawInput"]["command"], "cargo test --workspace");
        assert_eq!(
            json["content"][0]["content"]["text"],
            "cargo test --workspace"
        );
    }

    #[test]
    fn v1_tool_start_is_a_tool_call_creation_event() {
        let update = v1::ToolCallUpdate::try_from(tool_started(&json!({
            "call": {
                "tool_call_id": "call-1",
                "tool_name": "terminal",
                "raw_input": { "command": "pwd" }
            }
        })))
        .unwrap();
        let tool = v1::ToolCall::try_from(update).unwrap();
        let json = serde_json::to_value(v1::SessionUpdate::ToolCall(tool)).unwrap();
        assert_eq!(json["sessionUpdate"], "tool_call");
        assert_eq!(json["toolCallId"], "call-1");
        assert_eq!(json["status"], "in_progress");
    }

    #[test]
    fn v1_tool_update_remains_an_update_event() {
        let update = SessionUpdate::ToolCallUpdate(tool_started(&json!({
            "call": {
                "tool_call_id": "call-1",
                "tool_name": "terminal",
                "raw_input": { "command": "pwd" },
                "status": "in_progress"
            }
        })));
        let updates = Vec::<v1::SessionUpdate>::try_from(update).unwrap();
        let json = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(json["sessionUpdate"], "tool_call_update");
        assert_eq!(json["toolCallId"], "call-1");
        assert_eq!(json["status"], "in_progress");
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
        assert_eq!(json["kind"], "other");
        assert_eq!(json["rawInput"]["patch"], "*** Begin Patch\n*** End Patch");
    }
}
