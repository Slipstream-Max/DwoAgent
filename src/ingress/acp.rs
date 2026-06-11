//! ACP adapter for the agent runtime.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use agent_client_protocol::schema::{
    AgentCapabilities, CancelNotification, ConfigOptionUpdate, ContentBlock, ContentChunk,
    CurrentModeUpdate, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, PromptCapabilities, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, SessionCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionId,
    SessionInfoUpdate, SessionListCapabilities, SessionMode, SessionModeState, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    UsageUpdate,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Responder, on_receive_notification,
    on_receive_request,
};
use anyhow::{Context as AnyhowContext, Result};
use futures::io::AsyncRead;
use serde_json::{Map, Value};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::agent::activity::event::{
    EVENT_ACTIVITY_BOX, EVENT_ACTIVITY_BOX_UPDATE, EVENT_AGENT_MESSAGE_CHUNK,
    EVENT_AGENT_THOUGHT_CHUNK, EVENT_CONFIG_OPTION, EVENT_CURRENT_MODE, EVENT_SESSION_INFO,
    EVENT_TOOL_CALL, EVENT_TOOL_CALL_UPDATE, EVENT_USAGE_UPDATE, EVENT_USER_MESSAGE_CHUNK,
};
use crate::agent::constants::{
    MODE_ALLOW_ALL, MODE_BLOCK_ALL, MODE_CONFIRM, STOP_CANCELLED, STOP_COMPLETED, STOP_MAX_TURNS,
};
use crate::agent::service::AgentService;
use crate::agent::session::SESSION_CLIENT_TRANSCRIPT_FILE;
use crate::config::models::{ContextUsageSnapshot, ModelProfile, SessionTranscriptEvent};
use crate::context::content_block;
use crate::tools::subagent_tool_runtime::{PermissionRequester, UpdateEmitter};

/// Run the agent over ACP stdio transport.
pub async fn run_acp_stdio(agent: Arc<AgentService>) -> Result<()> {
    let stdin = EofAsError::new(tokio::io::stdin().compat());
    let stdout = tokio::io::stdout().compat_write();
    let transport = ByteStreams::new(stdout, stdin);

    let agent_fut = run_acp_transport(agent, transport);

    tokio::select! {
        result = agent_fut => {
            match result {
                Ok(()) => Ok(()),
                Err(err) if is_stdio_eof_error(&err) => {
                    tracing::info!("ACP stdio input closed, shutting down");
                    Ok(())
                }
                Err(err) => Err(anyhow::anyhow!("ACP connection error: {err}")),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received SIGINT, shutting down gracefully");
            Ok(())
        }
    }
}

/// Run the ACP agent over any line-delimited JSON-RPC transport.
pub async fn run_acp_transport<T>(
    agent: Arc<AgentService>,
    transport: T,
) -> std::result::Result<(), agent_client_protocol::Error>
where
    T: agent_client_protocol::ConnectTo<Agent> + 'static,
{
    let agent_for_init = agent.clone();
    let agent_for_new = agent.clone();
    let agent_for_prompt = agent.clone();
    let agent_for_cancel = agent.clone();
    let agent_for_list = agent.clone();
    let agent_for_load = agent.clone();
    let agent_for_mode = agent.clone();
    let agent_for_config = agent.clone();

    Agent
        .builder()
        .name(&agent.meta().name)
        // ── initialize ─────────────────────────────────────────────
        .on_receive_request(
            async move |req: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _cx: ConnectionTo<Client>| {
                let meta = agent_for_init.meta();
                let response = InitializeResponse::new(req.protocol_version)
                    .agent_info(agent_client_protocol::schema::Implementation::new(
                        &meta.name, "0.1.0",
                    ))
                    .agent_capabilities(
                        AgentCapabilities::new()
                            .load_session(true)
                            .prompt_capabilities(
                                PromptCapabilities::new()
                                    .image(true)
                                    .audio(false)
                                    .embedded_context(true),
                            )
                            .session_capabilities(
                                SessionCapabilities::new().list(SessionListCapabilities::new()),
                            ),
                    );
                responder.respond(response)
            },
            on_receive_request!(),
        )
        // ── session/new ────────────────────────────────────────────
        .on_receive_request(
            async move |req: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let cwd = req.cwd.to_string_lossy().to_string();
                match agent_for_new.new_session(&cwd).await {
                    Ok(session) => {
                        let snapshot = session.session_meta_snapshot().await;
                        let usage = session.context_usage_snapshot().await;
                        let profiles = agent_for_new.model_profiles();
                        let session_id = session.session_id().to_string();
                        let sid = SessionId::new(session_id.as_str());
                        let response = NewSessionResponse::new(sid)
                            .modes(build_mode_state(snapshot.mode_id.as_str()))
                            .config_options(build_config_options(
                                &snapshot.model_id,
                                snapshot.mode_id.as_str(),
                                snapshot.reasoning_mode.as_str(),
                                &profiles,
                            ));
                        let result = responder.respond(response);
                        emit_context_usage_state(&cx, &session_id, usage);
                        result
                    }
                    Err(err) => responder.respond_with_error(
                        agent_client_protocol::schema::Error::internal_error()
                            .data(format!("{err:#}")),
                    ),
                }
            },
            on_receive_request!(),
        )
        // ── session/prompt ─────────────────────────────────────────
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let session_id = req.session_id.to_string();
                let (user_input, user_blocks) = match normalize_prompt_blocks(&req.prompt) {
                    Ok(normalized) => normalized,
                    Err(err) => {
                        return responder.respond_with_error(
                            agent_client_protocol::schema::Error::invalid_params()
                                .data(format!("{err:#}")),
                        );
                    }
                };

                let cx_for_emit = cx.clone();
                let session_id_for_emit = session_id.clone();
                let emit_update: UpdateEmitter =
                    Arc::new(move |_target: String, update: Map<String, Value>| {
                        let cx = cx_for_emit.clone();
                        let sid = session_id_for_emit.clone();
                        Box::pin(async move {
                            emit_session_update(&cx, &sid, &update);
                            Ok(())
                        })
                    });

                let cx_for_perm = cx.clone();
                let session_id_for_perm = session_id.clone();
                let request_permission: PermissionRequester =
                    Arc::new(move |_target: String, payload: Map<String, Value>| {
                        let cx = cx_for_perm.clone();
                        let sid = session_id_for_perm.clone();
                        Box::pin(async move {
                            request_permission_from_client(&cx, &sid, &payload).await
                        })
                    });

                let cx_for_finish = cx.clone();
                cx.spawn({
                    let agent = agent_for_prompt.clone();
                    async move {
                        let result = agent
                            .run_prompt(
                                &session_id,
                                user_input,
                                user_blocks,
                                emit_update,
                                request_permission,
                            )
                            .await;

                        if result.is_ok()
                            && let Some(session) = agent.get_session(&session_id).await
                        {
                            let snapshot = session.session_meta_snapshot().await;
                            let usage = session.context_usage_snapshot().await;
                            let profiles = agent.model_profiles();
                            emit_mode_and_config_state(
                                &cx_for_finish,
                                &session_id,
                                snapshot.mode_id.as_str(),
                                &snapshot.model_id,
                                snapshot.reasoning_mode.as_str(),
                                &profiles,
                            );
                            emit_context_usage_state(&cx_for_finish, &session_id, usage);
                        }

                        respond_prompt_result(responder, result)
                    }
                })?;
                Ok(())
            },
            on_receive_request!(),
        )
        // ── session/cancel (notification) ──────────────────────────
        .on_receive_notification(
            async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                let session_id = notif.session_id.to_string();
                agent_for_cancel.cancel(&session_id).await;
                Ok(())
            },
            on_receive_notification!(),
        )
        // ── session/list ───────────────────────────────────────────
        .on_receive_request(
            async move |req: ListSessionsRequest,
                        responder: Responder<ListSessionsResponse>,
                        _cx: ConnectionTo<Client>| {
                // Cursor-based pagination: if cursor is provided, return empty (no next page).
                if req.cursor.is_some() {
                    let response = ListSessionsResponse::new(Vec::new());
                    return responder.respond(response);
                }
                let cwd = req.cwd.as_deref().map(|p| p.to_string_lossy().to_string());
                let sessions = agent_for_list.list_sessions(cwd.as_deref()).await;
                let items: Vec<agent_client_protocol::schema::SessionInfo> = sessions
                    .into_iter()
                    .map(|item| {
                        let mut info = agent_client_protocol::schema::SessionInfo::new(
                            SessionId::new(item.session_id.as_str()),
                            std::path::PathBuf::from(&item.cwd),
                        );
                        if let Some(title) = item.title {
                            info = info.title(title);
                        }
                        if let Some(updated_at) = item.updated_at {
                            info = info.updated_at(updated_at);
                        }
                        info
                    })
                    .collect();
                let response = ListSessionsResponse::new(items);
                responder.respond(response)
            },
            on_receive_request!(),
        )
        // ── session/load ───────────────────────────────────────────
        .on_receive_request(
            async move |req: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let session_id = req.session_id.to_string();
                match agent_for_load.load_session(&session_id).await {
                    Ok(Some(session)) => {
                        let snapshot = session.session_meta_snapshot().await;
                        let usage = session.context_usage_snapshot().await;
                        let profiles = agent_for_load.model_profiles();

                        // Replay transcript events.
                        let transcript_path =
                            session.session_dir().join(SESSION_CLIENT_TRANSCRIPT_FILE);
                        if let Err(err) = replay_transcript_file(&cx, &session_id, &transcript_path)
                        {
                            return responder.respond_with_error(
                                agent_client_protocol::schema::Error::internal_error()
                                    .data(format!("{err:#}")),
                            );
                        }

                        // Emit session info if title is set.
                        if let Some(title) = &snapshot.title {
                            let info_update = SessionInfoUpdate::new()
                                .title(title.clone())
                                .updated_at(snapshot.updated_at.clone().unwrap_or_default());
                            let notif = SessionNotification::new(
                                SessionId::new(session_id.as_str()),
                                SessionUpdate::SessionInfoUpdate(info_update),
                            );
                            let _ = cx.send_notification_to(Client, notif);
                        }

                        let response = LoadSessionResponse::new()
                            .modes(build_mode_state(snapshot.mode_id.as_str()))
                            .config_options(build_config_options(
                                &snapshot.model_id,
                                snapshot.mode_id.as_str(),
                                snapshot.reasoning_mode.as_str(),
                                &profiles,
                            ));
                        let result = responder.respond(response);
                        emit_context_usage_state(&cx, &session_id, usage);
                        result
                    }
                    Ok(None) => responder.respond_with_error(
                        agent_client_protocol::schema::Error::invalid_params()
                            .data("session not found"),
                    ),
                    Err(err) => responder.respond_with_error(
                        agent_client_protocol::schema::Error::internal_error()
                            .data(format!("{err:#}")),
                    ),
                }
            },
            on_receive_request!(),
        )
        // ── session/set_mode ───────────────────────────────────────
        .on_receive_request(
            async move |req: SetSessionModeRequest,
                        responder: Responder<SetSessionModeResponse>,
                        cx: ConnectionTo<Client>| {
                let session_id = req.session_id.to_string();
                let mode_id = req.mode_id.to_string();
                match agent_for_mode.set_session_mode(&session_id, &mode_id).await {
                    Ok(Some(session)) => {
                        let snapshot = session.session_meta_snapshot().await;
                        let profiles = agent_for_mode.model_profiles();

                        // Emit mode update notification.
                        let mode_notif = SessionNotification::new(
                            SessionId::new(session_id.as_str()),
                            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                                snapshot.mode_id.as_str(),
                            )),
                        );
                        let _ = cx.send_notification_to(Client, mode_notif);

                        // Emit config option update notification.
                        let config_notif = SessionNotification::new(
                            SessionId::new(session_id.as_str()),
                            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                                build_config_options(
                                    &snapshot.model_id,
                                    snapshot.mode_id.as_str(),
                                    snapshot.reasoning_mode.as_str(),
                                    &profiles,
                                ),
                            )),
                        );
                        let _ = cx.send_notification_to(Client, config_notif);

                        let result = responder.respond(SetSessionModeResponse::new());
                        spawn_context_usage_update(&cx, session_id, session);
                        result
                    }
                    Ok(None) => responder.respond_with_error(
                        agent_client_protocol::schema::Error::invalid_params()
                            .data("session not found"),
                    ),
                    Err(err) => responder.respond_with_error(
                        agent_client_protocol::schema::Error::invalid_params()
                            .data(format!("{err:#}")),
                    ),
                }
            },
            on_receive_request!(),
        )
        // ── session/set_config_option ──────────────────────────────
        .on_receive_request(
            async move |req: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        cx: ConnectionTo<Client>| {
                let session_id = req.session_id.to_string();
                let config_id = req.config_id.to_string();
                let value = req.value.to_string();

                let result = match config_id.as_str() {
                    "policy_mode" => {
                        match agent_for_config.set_session_mode(&session_id, &value).await {
                            Ok(Some(session)) => {
                                let snapshot = session.session_meta_snapshot().await;
                                // Emit mode update.
                                let mode_notif = SessionNotification::new(
                                    SessionId::new(session_id.as_str()),
                                    SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                                        snapshot.mode_id.as_str(),
                                    )),
                                );
                                let _ = cx.send_notification_to(Client, mode_notif);
                                Ok(snapshot)
                            }
                            Ok(None) => Err(anyhow::anyhow!("session not found")),
                            Err(e) => Err(e),
                        }
                    }
                    "model" => {
                        match agent_for_config
                            .set_session_model(&session_id, &value)
                            .await
                        {
                            Ok(_) => match agent_for_config.get_session(&session_id).await {
                                Some(session) => Ok(session.session_meta_snapshot().await),
                                None => Err(anyhow::anyhow!("session not found")),
                            },
                            Err(e) => Err(e),
                        }
                    }
                    "reasoning_mode" => {
                        match agent_for_config
                            .set_session_reasoning_mode(&session_id, &value)
                            .await
                        {
                            Ok(_) => match agent_for_config.get_session(&session_id).await {
                                Some(session) => Ok(session.session_meta_snapshot().await),
                                None => Err(anyhow::anyhow!("session not found")),
                            },
                            Err(e) => Err(e),
                        }
                    }
                    _ => Err(anyhow::anyhow!("Unsupported config option: {config_id}")),
                };

                match result {
                    Ok(snapshot) => {
                        let profiles = agent_for_config.model_profiles();
                        let current_model_id = snapshot
                            .pending_model_id
                            .as_deref()
                            .unwrap_or(&snapshot.model_id);
                        let current_reasoning_mode = snapshot
                            .pending_reasoning_mode
                            .unwrap_or(snapshot.reasoning_mode);
                        let options = build_config_options(
                            current_model_id,
                            snapshot.mode_id.as_str(),
                            current_reasoning_mode.as_str(),
                            &profiles,
                        );
                        let session_for_usage = agent_for_config.get_session(&session_id).await;
                        let response = SetSessionConfigOptionResponse::new(options);
                        let result = responder.respond(response);
                        if let Some(session) = session_for_usage {
                            spawn_context_usage_update(&cx, session_id, session);
                        }
                        result
                    }
                    Err(err) => responder.respond_with_error(
                        agent_client_protocol::schema::Error::invalid_params()
                            .data(format!("{err:#}")),
                    ),
                }
            },
            on_receive_request!(),
        )
        .connect_to(transport)
        .await
}

struct EofAsError<R> {
    inner: R,
}

impl<R> EofAsError<R> {
    fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R> AsyncRead for EofAsError<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(0)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ACP stdio input closed",
            ))),
            other => other,
        }
    }
}

fn is_stdio_eof_error(err: &agent_client_protocol::Error) -> bool {
    format!("{err:?}").contains("ACP stdio input closed")
}

// ── Config/Mode/Model state builders ───────────────────────────────────────

fn emit_mode_and_config_state(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    current_mode_id: &str,
    current_model_id: &str,
    current_reasoning_mode: &str,
    model_profiles: &HashMap<String, ModelProfile>,
) {
    let sid = SessionId::new(session_id);
    let mode_notif = SessionNotification::new(
        sid.clone(),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(current_mode_id.to_string())),
    );
    let _ = cx.send_notification_to(Client, mode_notif);

    let config_notif = SessionNotification::new(
        sid,
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(build_config_options(
            current_model_id,
            current_mode_id,
            current_reasoning_mode,
            model_profiles,
        ))),
    );
    let _ = cx.send_notification_to(Client, config_notif);
}

fn emit_context_usage_state(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    usage: ContextUsageSnapshot,
) {
    let notif = SessionNotification::new(
        SessionId::new(session_id),
        SessionUpdate::UsageUpdate(UsageUpdate::new(usage.used, usage.size)),
    );
    let _ = cx.send_notification_to(Client, notif);
}

fn spawn_context_usage_update(
    cx: &ConnectionTo<Client>,
    session_id: String,
    session: Arc<crate::agent::session_agent::SessionAgent>,
) {
    let cx_for_usage = cx.clone();
    let _ = cx.spawn(async move {
        let usage = session.context_usage_snapshot().await;
        emit_context_usage_state(&cx_for_usage, &session_id, usage);
        Ok(())
    });
}

fn build_mode_state(current_mode_id: &str) -> SessionModeState {
    SessionModeState::new(
        current_mode_id.to_string(),
        vec![
            SessionMode::new(MODE_ALLOW_ALL, "Allow All")
                .description("Allow all tool calls without confirmation.".to_string()),
            SessionMode::new(MODE_BLOCK_ALL, "Block All")
                .description("Block all tool calls.".to_string()),
            SessionMode::new(MODE_CONFIRM, "Confirm")
                .description("Ask for permission before each tool call.".to_string()),
        ],
    )
}

fn build_config_options(
    current_model_id: &str,
    current_mode_id: &str,
    current_reasoning_mode: &str,
    model_profiles: &HashMap<String, ModelProfile>,
) -> Vec<SessionConfigOption> {
    let current_profile = model_profiles.get(current_model_id);

    // Policy mode selector.
    let policy_option = SessionConfigOption::select(
        "policy_mode",
        "Policy",
        current_mode_id.to_string(),
        vec![
            SessionConfigSelectOption::new(MODE_CONFIRM, "Confirm")
                .description("Ask for permission before each tool call.".to_string()),
            SessionConfigSelectOption::new(MODE_ALLOW_ALL, "Allow All")
                .description("Allow all tool calls without confirmation.".to_string()),
            SessionConfigSelectOption::new(MODE_BLOCK_ALL, "Block All")
                .description("Block all tool calls.".to_string()),
        ],
    )
    .description("Choose how tool calls are handled.".to_string())
    .category(SessionConfigOptionCategory::Mode);

    // Model selector.
    let model_options: Vec<SessionConfigSelectOption> = model_profiles
        .values()
        .map(|profile| {
            SessionConfigSelectOption::new(profile.model_name.clone(), profile.model_name.clone())
                .description(model_description(profile))
        })
        .collect();
    let model_option = SessionConfigOption::select(
        "model",
        "Model",
        current_model_id.to_string(),
        model_options,
    )
    .description("Choose which model this session uses.".to_string())
    .category(SessionConfigOptionCategory::Model);

    // Reasoning mode selector.
    let reasoning_options: Vec<SessionConfigSelectOption> = current_profile
        .map(|p| {
            p.reasoning_modes
                .iter()
                .map(|mode| {
                    let mode_str = mode.as_str().to_string();
                    SessionConfigSelectOption::new(mode_str.clone(), mode_str.clone())
                        .description(format!("{} reasoning: {}", current_model_id, mode_str))
                })
                .collect()
        })
        .unwrap_or_default();
    let reasoning_option = SessionConfigOption::select(
        "reasoning_mode",
        "Reasoning Mode",
        current_reasoning_mode.to_string(),
        reasoning_options,
    )
    .description("Choose the reasoning mode for this session.".to_string())
    .category(SessionConfigOptionCategory::ThoughtLevel);

    vec![policy_option, model_option, reasoning_option]
}

fn model_description(profile: &ModelProfile) -> String {
    let vision = if profile.capabilities.vision {
        "yes"
    } else {
        "no"
    };
    let tools = if profile.capabilities.tool_use {
        "yes"
    } else {
        "no"
    };
    format!(
        "{}/{} | vision: {} | tools: {}",
        profile.config.provider, profile.config.model_id, vision, tools
    )
}

// ── Permission request ─────────────────────────────────────────────────────

async fn request_permission_from_client(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    payload: &Map<String, Value>,
) -> Result<String> {
    let tool_call_id: ToolCallId = payload
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
        .into();

    // Build ToolCallUpdate with fields populated from the payload.
    let mut fields = ToolCallUpdateFields::new();
    if let Some(status) = payload.get("status").and_then(Value::as_str) {
        fields = fields.status(parse_tool_call_status(status));
    }
    if let Some(title) = payload.get("title").and_then(Value::as_str) {
        fields = fields.title(title.to_string());
    }
    if let Some(kind) = payload.get("kind").and_then(Value::as_str) {
        fields = fields.kind(parse_tool_kind(kind));
    }
    if let Some(raw_input) = payload.get("raw_input") {
        fields = fields.raw_input(raw_input.clone());
    }
    if let Some(raw_output) = payload.get("raw_output") {
        fields = fields.raw_output(raw_output.clone());
    }
    if let Some(content) = render_tool_call_content(
        payload.get("title").and_then(Value::as_str).unwrap_or(""),
        payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending"),
        payload.get("raw_input"),
        payload.get("raw_output"),
    ) {
        fields = fields.content(content);
    }
    let tool_call = ToolCallUpdate::new(tool_call_id, fields);

    let options = vec![
        PermissionOption::new("allow_once", "Allow Once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "reject_once",
            "Reject Once",
            PermissionOptionKind::RejectOnce,
        ),
    ];

    let sid = SessionId::new(session_id);
    let request = RequestPermissionRequest::new(sid, tool_call, options);

    let response = cx
        .send_request_to(Client, request)
        .block_task()
        .await
        .map_err(|e| anyhow::anyhow!("permission request failed: {e}"))?;

    match response.outcome {
        RequestPermissionOutcome::Cancelled => Ok("cancelled".to_string()),
        RequestPermissionOutcome::Selected(selected) => {
            let option_id = selected.option_id.to_string();
            match option_id.as_str() {
                "allow_once" | "reject_once" => Ok(option_id),
                _ => Ok("reject_once".to_string()),
            }
        }
        _ => Ok("reject_once".to_string()),
    }
}

// ── Transcript replay ──────────────────────────────────────────────────────

fn replay_transcript_file(cx: &ConnectionTo<Client>, session_id: &str, path: &Path) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("open {}", path.display())),
    };

    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let event: SessionTranscriptEvent = serde_json::from_str(text)
            .with_context(|| format!("parse transcript event in {}", path.display()))?;
        emit_session_update(cx, session_id, &event.update);
    }
    Ok(())
}

// ── Prompt normalization ───────────────────────────────────────────────────

fn normalize_prompt_blocks(prompt: &[ContentBlock]) -> Result<(Value, Vec<Value>)> {
    if prompt.is_empty() {
        anyhow::bail!("prompt cannot be empty");
    }

    let mut blocks: Vec<Value> = Vec::new();
    for (index, item) in prompt.iter().enumerate() {
        match item {
            ContentBlock::Text(text) => {
                let text_value = text.text.trim();
                if text_value.is_empty() {
                    continue;
                }
                blocks.push(content_block::text(text_value)?);
            }
            ContentBlock::Image(img) => {
                if img.data.trim().is_empty()
                    && img.uri.as_deref().map(str::trim).unwrap_or("").is_empty()
                {
                    anyhow::bail!("image block at index {index} must provide either data or uri");
                }
                if !img.data.trim().is_empty() {
                    if img.mime_type.trim().is_empty() {
                        anyhow::bail!("image block at index {index} must provide mimeType");
                    }
                    blocks.push(content_block::image_url_data(&img.mime_type, &img.data)?);
                    continue;
                }
                if let Some(uri) = img
                    .uri
                    .as_deref()
                    .map(str::trim)
                    .filter(|uri| !uri.is_empty())
                {
                    blocks.push(content_block::image_url(uri)?);
                }
            }
            ContentBlock::Resource(_) | ContentBlock::ResourceLink(_) => {
                blocks.push(serde_json::to_value(item)?);
            }
            _ => {
                anyhow::bail!("Unsupported prompt block type at index {index}");
            }
        }
    }

    if blocks.is_empty() {
        anyhow::bail!("prompt cannot be empty");
    }

    if blocks.len() == 1 {
        if let Some(text) = blocks[0].get("text").and_then(Value::as_str) {
            return Ok((Value::String(text.to_string()), blocks));
        }
    }
    Ok((Value::Array(blocks.clone()), blocks))
}

fn map_stop_reason(stop_reason: &str) -> StopReason {
    match stop_reason {
        STOP_COMPLETED => StopReason::EndTurn,
        STOP_CANCELLED => StopReason::Cancelled,
        STOP_MAX_TURNS => StopReason::MaxTurnRequests,
        _ => StopReason::EndTurn,
    }
}

fn respond_prompt_result(
    responder: Responder<PromptResponse>,
    result: Result<String>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    match result {
        Ok(stop_reason) => responder.respond(PromptResponse::new(map_stop_reason(&stop_reason))),
        Err(err) => responder.respond_with_error(
            agent_client_protocol::schema::Error::internal_error().data(format!("{err:#}")),
        ),
    }
}

// ── Session update emission ────────────────────────────────────────────────

fn emit_session_update(cx: &ConnectionTo<Client>, session_id: &str, update: &Map<String, Value>) {
    let session_update_type = update
        .get("session_update")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    let sid = SessionId::new(session_id);

    let notification = match session_update_type {
        s if s == EVENT_AGENT_MESSAGE_CHUNK => {
            let text = extract_content_text(update);
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            Some(SessionNotification::new(
                sid,
                SessionUpdate::AgentMessageChunk(chunk),
            ))
        }
        s if s == EVENT_AGENT_THOUGHT_CHUNK => {
            let text = extract_content_text(update);
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            Some(SessionNotification::new(
                sid,
                SessionUpdate::AgentThoughtChunk(chunk),
            ))
        }
        s if s == EVENT_USER_MESSAGE_CHUNK => {
            let text = extract_content_text(update);
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            Some(SessionNotification::new(
                sid,
                SessionUpdate::UserMessageChunk(chunk),
            ))
        }
        s if s == EVENT_TOOL_CALL => {
            let tool_call_id: ToolCallId = update
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into();
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            let mut tc = ToolCall::new(tool_call_id, title.clone())
                .kind(parse_tool_kind(
                    update
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("other"),
                ))
                .status(parse_tool_call_status(status));
            if let Some(raw_input) = update.get("raw_input") {
                tc = tc.raw_input(raw_input.clone());
            }
            if let Some(raw_output) = update.get("raw_output") {
                tc = tc.raw_output(raw_output.clone());
            }
            if let Some(content) = update
                .get("content")
                .and_then(parse_tool_call_content)
                .or_else(|| {
                    render_tool_call_content(
                        &title,
                        status,
                        update.get("raw_input"),
                        update.get("raw_output"),
                    )
                })
            {
                tc = tc.content(content);
            }
            Some(SessionNotification::new(sid, SessionUpdate::ToolCall(tc)))
        }
        s if s == EVENT_TOOL_CALL_UPDATE => {
            let tool_call_id: ToolCallId = update
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into();
            let mut fields = ToolCallUpdateFields::new();
            if let Some(status) = update.get("status").and_then(Value::as_str) {
                fields = fields.status(parse_tool_call_status(status));
            }
            if let Some(title) = update.get("title").and_then(Value::as_str) {
                fields = fields.title(title.to_string());
            }
            if let Some(kind) = update.get("kind").and_then(Value::as_str) {
                fields = fields.kind(parse_tool_kind(kind));
            }
            if let Some(raw_input) = update.get("raw_input") {
                fields = fields.raw_input(raw_input.clone());
            }
            if let Some(raw_output) = update.get("raw_output") {
                fields = fields.raw_output(raw_output.clone());
            }
            if let Some(content) = update
                .get("content")
                .and_then(parse_tool_call_content)
                .or_else(|| {
                    render_tool_call_content(
                        update.get("title").and_then(Value::as_str).unwrap_or(""),
                        update
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("pending"),
                        update.get("raw_input"),
                        update.get("raw_output"),
                    )
                })
            {
                fields = fields.content(content);
            }
            let tcu = ToolCallUpdate::new(tool_call_id, fields);
            Some(SessionNotification::new(
                sid,
                SessionUpdate::ToolCallUpdate(tcu),
            ))
        }
        s if s == EVENT_ACTIVITY_BOX => {
            let tool_call_id: ToolCallId = update
                .get("activity_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into();
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Activity")
                .to_string();
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("in_progress");
            let mut tc = ToolCall::new(tool_call_id, title)
                .kind(parse_tool_kind(
                    update
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("think"),
                ))
                .status(parse_tool_call_status(status));
            if let Some(content) = update.get("content").and_then(parse_tool_call_content) {
                tc = tc.content(content);
            }
            Some(SessionNotification::new(sid, SessionUpdate::ToolCall(tc)))
        }
        s if s == EVENT_ACTIVITY_BOX_UPDATE => {
            let tool_call_id: ToolCallId = update
                .get("activity_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into();
            let mut fields = ToolCallUpdateFields::new();
            if let Some(status) = update.get("status").and_then(Value::as_str) {
                fields = fields.status(parse_tool_call_status(status));
            }
            if let Some(title) = update.get("title").and_then(Value::as_str) {
                fields = fields.title(title.to_string());
            }
            if let Some(kind) = update.get("kind").and_then(Value::as_str) {
                fields = fields.kind(parse_tool_kind(kind));
            }
            if let Some(content) = update.get("content").and_then(parse_tool_call_content) {
                fields = fields.content(content);
            }
            let tcu = ToolCallUpdate::new(tool_call_id, fields);
            Some(SessionNotification::new(
                sid,
                SessionUpdate::ToolCallUpdate(tcu),
            ))
        }
        s if s == EVENT_CURRENT_MODE => {
            let mode_id = update
                .get("current_mode_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(SessionNotification::new(
                sid,
                SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(mode_id)),
            ))
        }
        s if s == EVENT_CONFIG_OPTION => {
            // Emit with empty options — the full config is sent via response payloads.
            Some(SessionNotification::new(
                sid,
                SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(Vec::new())),
            ))
        }
        s if s == EVENT_USAGE_UPDATE => {
            let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
            let size = update.get("size").and_then(Value::as_u64).unwrap_or(0);
            Some(SessionNotification::new(
                sid,
                SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)),
            ))
        }
        s if s == EVENT_SESSION_INFO => {
            let mut info_update = SessionInfoUpdate::new();
            if let Some(title) = update.get("title").and_then(Value::as_str) {
                info_update = info_update.title(title.to_string());
            }
            if let Some(updated_at) = update.get("updated_at").and_then(Value::as_str) {
                info_update = info_update.updated_at(updated_at.to_string());
            }
            Some(SessionNotification::new(
                sid,
                SessionUpdate::SessionInfoUpdate(info_update),
            ))
        }
        _ => None,
    };

    if let Some(notif) = notification {
        let _ = cx.send_notification_to(Client, notif);
    }
}

fn extract_content_text(update: &Map<String, Value>) -> String {
    update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn parse_tool_call_status(status: &str) -> ToolCallStatus {
    match status.trim() {
        "pending" => ToolCallStatus::Pending,
        "in_progress" => ToolCallStatus::InProgress,
        "completed" | "completed_success" => ToolCallStatus::Completed,
        "failed" | "completed_error" => ToolCallStatus::Failed,
        _ => ToolCallStatus::Pending,
    }
}

fn parse_tool_kind(kind: &str) -> ToolKind {
    match kind.trim() {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        "switch_mode" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

fn render_tool_call_content(
    _title: &str,
    _status: &str,
    raw_input: Option<&Value>,
    raw_output: Option<&Value>,
) -> Option<Vec<ToolCallContent>> {
    if let Some(output) = raw_output {
        let result_text = format!("```json\n{}\n```", format_json_like(output));
        return Some(vec![ToolCallContent::from(ContentBlock::Text(
            TextContent::new(truncate_text(&result_text, 8000)),
        ))]);
    }

    let mut text = None;
    if let Some(Value::Object(input)) = raw_input {
        if let Some(command) = input.get("command").and_then(Value::as_str)
            && !command.trim().is_empty()
        {
            text = Some(command.to_string());
        }
    }
    if text.is_none() {
        text = raw_input.map(format_json_like);
    }
    text.filter(|s| !s.trim().is_empty()).map(|value| {
        vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
            truncate_text(&value, 8000),
        )))]
    })
}

fn parse_tool_call_content(value: &Value) -> Option<Vec<ToolCallContent>> {
    let items = value.as_array()?;
    let mut out = Vec::new();
    for item in items {
        let content = item.get("content").unwrap_or(item);
        let content_type = content.get("type").and_then(Value::as_str).unwrap_or("");
        if content_type != "text" {
            continue;
        }
        let text = content.get("text").and_then(Value::as_str).unwrap_or("");
        if text.trim().is_empty() {
            continue;
        }
        out.push(ToolCallContent::from(ContentBlock::Text(TextContent::new(
            text.to_string(),
        ))));
    }
    if out.is_empty() { None } else { Some(out) }
}

fn format_json_like(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("\n[TRUNCATED]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ImageContent;
    use serde_json::json;

    #[test]
    fn normalize_prompt_blocks_rejects_empty_prompt() {
        let error = normalize_prompt_blocks(&[]).unwrap_err();
        assert!(error.to_string().contains("prompt cannot be empty"));
    }

    #[test]
    fn normalize_prompt_blocks_rejects_only_empty_text() {
        let prompt = vec![ContentBlock::Text(TextContent::new("   "))];

        let error = normalize_prompt_blocks(&prompt).unwrap_err();

        assert!(error.to_string().contains("prompt cannot be empty"));
    }

    #[test]
    fn normalize_prompt_blocks_ignores_empty_text_around_resource_link() {
        let prompt = vec![
            ContentBlock::Text(TextContent::new("   ")),
            serde_json::from_value(json!({
                "type": "resource_link",
                "uri": "file:///tmp/example.md",
                "name": "example.md",
                "mimeType": "text/markdown"
            }))
            .unwrap(),
            ContentBlock::Text(TextContent::new("  summarize this  ")),
        ];

        let (user_input, user_blocks) = normalize_prompt_blocks(&prompt).unwrap();

        assert_eq!(user_input, Value::Array(user_blocks.clone()));
        assert_eq!(user_blocks[0]["type"], "resource_link");
        assert_eq!(
            user_blocks[1],
            json!({"type": "text", "text": "summarize this"})
        );
    }

    #[test]
    fn normalize_prompt_blocks_accepts_single_text_as_string_input() {
        let prompt = vec![ContentBlock::Text(TextContent::new("hello"))];

        let (user_input, user_blocks) = normalize_prompt_blocks(&prompt).unwrap();

        assert_eq!(user_input, Value::String("hello".to_string()));
        assert_eq!(user_blocks, vec![json!({"type": "text", "text": "hello"})]);
    }

    #[test]
    fn normalize_prompt_blocks_rejects_image_without_data_or_uri() {
        let prompt = vec![ContentBlock::Image(ImageContent::new("", ""))];

        let error = normalize_prompt_blocks(&prompt).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("image block at index 0 must provide either data or uri")
        );
    }

    #[test]
    fn normalize_prompt_blocks_converts_image_data_to_image_url() {
        let prompt = vec![ContentBlock::Image(ImageContent::new("abc", "image/png"))];

        let (user_input, user_blocks) = normalize_prompt_blocks(&prompt).unwrap();

        assert_eq!(user_input, Value::Array(user_blocks.clone()));
        assert_eq!(
            user_blocks,
            vec![json!({
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,abc"}
            })]
        );
    }

    #[test]
    fn render_tool_call_content_prefers_command_input() {
        let content = render_tool_call_content(
            "terminal_exec",
            "pending",
            Some(&json!({"command": "cargo check", "timeout": 30})),
            None,
        )
        .unwrap();

        let rendered = serde_json::to_value(content).unwrap();
        assert_eq!(rendered[0]["content"]["text"], "cargo check");
    }

    #[test]
    fn render_tool_call_content_wraps_raw_output_as_json() {
        let content = render_tool_call_content(
            "terminal_exec",
            "completed",
            None,
            Some(&json!({"status": "completed_success"})),
        )
        .unwrap();

        let rendered = serde_json::to_value(content).unwrap();
        let text = rendered[0]["content"]["text"].as_str().unwrap();
        assert!(text.starts_with("```json\n"));
        assert!(text.contains("\"status\": \"completed_success\""));
    }

    #[test]
    fn parse_tool_call_content_uses_explicit_markdown_content() {
        let content = parse_tool_call_content(&json!([{
            "type": "content",
            "content": {
                "type": "text",
                "text": "Agent Flow:\n\n[tool] terminal_exec"
            }
        }]))
        .unwrap();

        let rendered = serde_json::to_value(content).unwrap();
        assert_eq!(
            rendered[0]["content"]["text"],
            "Agent Flow:\n\n[tool] terminal_exec"
        );
    }
}
