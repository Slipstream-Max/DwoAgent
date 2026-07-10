//! Thin stdio handlers for ACP requests.

use std::sync::Arc;

use agent_client_protocol::schema::{
    AgentCapabilities, CancelNotification, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    SessionCapabilities, SessionId, SessionListCapabilities, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
};
use agent_client_protocol::{Client, ConnectionTo, Responder};

use crate::agent::service::AgentService;
use crate::agent::session::SESSION_CLIENT_TRANSCRIPT_FILE;
use crate::protocol::acp::{mapper, notifications, permissions, transcript};

pub async fn initialize(
    agent: Arc<AgentService>,
    req: InitializeRequest,
    responder: Responder<InitializeResponse>,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let meta = agent.meta();
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
}

pub async fn new_session(
    agent: Arc<AgentService>,
    req: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let cwd = req.cwd.to_string_lossy().to_string();
    match agent.new_session_with_options(&cwd, None, None).await {
        Ok(session) => {
            let snapshot = session.session_meta_snapshot().await;
            let usage = session.context_usage_snapshot().await;
            let profiles = agent.model_profiles();
            let session_id = session.session_id().to_string();
            let response = NewSessionResponse::new(SessionId::new(session_id.as_str()))
                .modes(mapper::build_mode_state(snapshot.mode_id.as_str()))
                .config_options(mapper::build_config_options(
                    &snapshot.model_id,
                    snapshot.mode_id.as_str(),
                    snapshot.reasoning_mode.as_str(),
                    &profiles,
                ));
            let result = responder.respond(response);
            notifications::emit_context_usage_state(&cx, &session_id, usage);
            result
        }
        Err(err) => responder.respond_with_error(internal_error(err)),
    }
}

pub async fn prompt(
    agent: Arc<AgentService>,
    req: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.to_string();
    let (user_input, user_blocks) = match mapper::normalize_prompt_blocks(&req.prompt) {
        Ok(normalized) => normalized,
        Err(err) => return responder.respond_with_error(invalid_params(err)),
    };

    let emit_update = notifications::update_emitter(&cx, &session_id);
    let request_permission = permissions::permission_requester(&cx, &session_id);
    let cx_for_finish = cx.clone();

    cx.spawn({
        let agent = agent.clone();
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
                notifications::emit_mode_and_config_state(
                    &cx_for_finish,
                    &session_id,
                    snapshot.mode_id.as_str(),
                    &snapshot.model_id,
                    snapshot.reasoning_mode.as_str(),
                    &profiles,
                );
                notifications::emit_context_usage_state(&cx_for_finish, &session_id, usage);
            }

            match result {
                Ok(stop_reason) => {
                    responder.respond(PromptResponse::new(mapper::map_stop_reason(&stop_reason)))
                }
                Err(err) => responder.respond_with_error(internal_error(err)),
            }
        }
    })?;
    Ok(())
}

pub async fn cancel(
    agent: Arc<AgentService>,
    notif: CancelNotification,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let session_id = notif.session_id.to_string();
    agent.cancel(&session_id).await;
    Ok(())
}

pub async fn list_sessions(
    agent: Arc<AgentService>,
    req: ListSessionsRequest,
    responder: Responder<ListSessionsResponse>,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    if req.cursor.is_some() {
        return responder.respond(ListSessionsResponse::new(Vec::new()));
    }
    let cwd = req.cwd.as_deref().map(|p| p.to_string_lossy().to_string());
    let items = agent
        .list_sessions(cwd.as_deref())
        .await
        .into_iter()
        .map(mapper::session_info)
        .collect();
    responder.respond(ListSessionsResponse::new(items))
}

pub async fn load_session(
    agent: Arc<AgentService>,
    req: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.to_string();
    match agent.load_session(&session_id).await {
        Ok(Some(session)) => {
            let snapshot = session.session_meta_snapshot().await;
            let profiles = agent.model_profiles();

            let response = LoadSessionResponse::new()
                .modes(mapper::build_mode_state(snapshot.mode_id.as_str()))
                .config_options(mapper::build_config_options(
                    &snapshot.model_id,
                    snapshot.mode_id.as_str(),
                    snapshot.reasoning_mode.as_str(),
                    &profiles,
                ));
            let result = responder.respond(response);
            if result.is_ok() {
                let transcript_path = session.session_dir().join(SESSION_CLIENT_TRANSCRIPT_FILE);
                if let Err(err) =
                    transcript::replay_transcript_file(&cx, &session_id, &transcript_path)
                {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %format!("{err:#}"),
                        "failed to replay ACP transcript after session/load"
                    );
                }

                notifications::emit_session_info_state(
                    &cx,
                    &session_id,
                    snapshot.title.as_deref(),
                    snapshot.updated_at.as_deref(),
                );
                match session.persisted_context_usage_snapshot() {
                    Ok(Some(usage)) => {
                        notifications::emit_context_usage_state(&cx, &session_id, usage)
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(
                            session_id = %session_id,
                            error = %format!("{err:#}"),
                            "failed to read persisted context usage after session/load"
                        );
                    }
                }
            }
            result
        }
        Ok(None) => responder.respond_with_error(invalid_params("session not found")),
        Err(err) => responder.respond_with_error(internal_error(err)),
    }
}

pub async fn set_session_mode(
    agent: Arc<AgentService>,
    req: SetSessionModeRequest,
    responder: Responder<SetSessionModeResponse>,
    cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.to_string();
    let mode_id = req.mode_id.to_string();
    match agent.set_session_mode(&session_id, &mode_id).await {
        Ok(Some(session)) => {
            let snapshot = session.session_meta_snapshot().await;
            let profiles = agent.model_profiles();
            notifications::emit_mode_and_config_state(
                &cx,
                &session_id,
                snapshot.mode_id.as_str(),
                &snapshot.model_id,
                snapshot.reasoning_mode.as_str(),
                &profiles,
            );
            let result = responder.respond(SetSessionModeResponse::new());
            notifications::spawn_context_usage_update(&cx, session_id, session);
            result
        }
        Ok(None) => responder.respond_with_error(invalid_params("session not found")),
        Err(err) => responder.respond_with_error(invalid_params(err)),
    }
}

pub async fn set_session_config_option(
    agent: Arc<AgentService>,
    req: SetSessionConfigOptionRequest,
    responder: Responder<SetSessionConfigOptionResponse>,
    cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.to_string();
    let config_id = req.config_id.to_string();
    let value = req.value.to_string();

    match agent
        .set_session_config_option(&session_id, &config_id, &value)
        .await
    {
        Ok(Some(snapshot)) => {
            if config_id == "policy_mode" {
                notifications::emit_current_mode_state(&cx, &session_id, snapshot.mode_id.as_str());
            }
            let profiles = agent.model_profiles();
            let options = mapper::build_config_options_for_snapshot(&snapshot, &profiles);
            let session_for_usage = agent.get_session(&session_id).await;
            let result = responder.respond(SetSessionConfigOptionResponse::new(options));
            if let Some(session) = session_for_usage {
                notifications::spawn_context_usage_update(&cx, session_id, session);
            }
            result
        }
        Ok(None) => responder.respond_with_error(invalid_params("session not found")),
        Err(err) => responder.respond_with_error(invalid_params(err)),
    }
}

fn internal_error(err: impl std::fmt::Display) -> agent_client_protocol::schema::Error {
    agent_client_protocol::schema::Error::internal_error().data(format!("{err:#}"))
}

fn invalid_params(err: impl std::fmt::Display) -> agent_client_protocol::schema::Error {
    agent_client_protocol::schema::Error::invalid_params().data(format!("{err:#}"))
}
