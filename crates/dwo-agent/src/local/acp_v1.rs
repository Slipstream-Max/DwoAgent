use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::{ProtocolVersion, v1, v2};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Responder, on_receive_notification,
    on_receive_request,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "initialize", response = v1::InitializeResponse)]
#[serde(rename_all = "camelCase")]
struct InitializeRequest {
    protocol_version: ProtocolVersion,
    #[serde(default)]
    client_capabilities: Value,
}

fn advertises_compaction(request: &InitializeRequest) -> bool {
    request
        .client_capabilities
        .pointer("/session/compaction")
        .is_some_and(Value::is_object)
}

pub(super) async fn run_with_io<R, W>(config_path: PathBuf, stdin: R, stdout: W) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let eof = CancellationToken::new();
    let transport = ByteStreams::new(
        stdout.compat_write(),
        EofReader::new(stdin, eof.clone()).compat(),
    );
    let runtime = AcpRuntime {
        config_path,
        observers: Arc::new(Mutex::new(HashMap::new())),
        prompt_waiters: Arc::new(Mutex::new(HashMap::new())),
        pending_cancels: PendingCancels::default(),
        compaction_supported: Arc::new(AtomicBool::new(false)),
    };
    let initialize_compaction_supported = runtime.compaction_supported.clone();
    let new_runtime = runtime.clone();
    let list_runtime = runtime.clone();
    let fork_runtime = runtime.clone();
    let load_runtime = runtime.clone();
    let resume_runtime = runtime.clone();
    let prompt_runtime = runtime.clone();
    let close_runtime = runtime.clone();
    let delete_runtime = runtime.clone();
    let set_runtime = runtime.clone();
    let cancel_runtime = runtime;

    let agent = Agent
        .builder()
        .on_receive_request(
            async move |request: InitializeRequest,
                        responder: Responder<v1::InitializeResponse>,
                        _cx: ConnectionTo<Client>| {
                initialize_compaction_supported
                    .store(advertises_compaction(&request), Ordering::Relaxed);
                let response = v2::InitializeResponse::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("dwo", env!("CARGO_PKG_VERSION")),
                )
                .capabilities(agent_capabilities());
                match v1::InitializeResponse::try_from(response) {
                    Ok(mut response) => {
                        response.protocol_version = ProtocolVersion::V1;
                        responder.respond(response)
                    }
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::NewSessionRequest,
                        responder: Responder<v1::NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let request = match v2::NewSessionRequest::try_from(request) {
                    Ok(request) => request,
                    Err(error) => return responder.respond_with_error(invalid_params(error)),
                };
                if let Err(error) = validate_new_session(&request) {
                    return responder.respond_with_error(invalid_params(error));
                }
                let value = match ipc::request(
                    &new_runtime.config_path,
                    "session.new",
                    json!({"cwd": request.cwd.into_inner(), "title": Value::Null}),
                )
                .await
                {
                    Ok(value) => value,
                    Err(error) => return responder.respond_with_error(internal_error(error)),
                };
                let id = value["session_id"].as_str().unwrap_or_default();
                let options = match session_config_options(&new_runtime.config_path, id).await {
                    Ok(options) => options,
                    Err(error) => return responder.respond_with_error(internal_error(error)),
                };
                let response = match v1::NewSessionResponse::try_from(
                    v2::NewSessionResponse::new(v2::SessionId::new(id)).config_options(options),
                ) {
                    Ok(response) => response,
                    Err(error) => return responder.respond_with_error(internal_error(error)),
                };
                let connection = AcpConnection::new(
                    AcpProtocol::V1,
                    cx,
                    new_runtime.compaction_supported.clone(),
                );
                let result = responder.respond(response);
                if result.is_ok() {
                    if let Some((used, size)) = snapshot_usage(&value) {
                        send_usage_update(&connection, id, used, size);
                    }
                    send_available_commands(&connection, id);
                }
                result
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::ForkSessionRequest,
                        responder: Responder<v1::ForkSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                let request = match v2::ForkSessionRequest::try_from(request) {
                    Ok(request) => request,
                    Err(error) => return responder.respond_with_error(invalid_params(error)),
                };
                if let Err(error) = validate_fork(&fork_runtime, &request).await {
                    return responder.respond_with_error(invalid_params(error));
                }
                match fork_acp_session(&fork_runtime, &request.session_id.to_string()).await {
                    Ok(response) => match v1::ForkSessionResponse::try_from(response) {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(internal_error(error)),
                    },
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::ListSessionsRequest,
                        responder: Responder<v1::ListSessionsResponse>,
                        _cx: ConnectionTo<Client>| {
                let request = match v2::ListSessionsRequest::try_from(request) {
                    Ok(request) => request,
                    Err(error) => return responder.respond_with_error(invalid_params(error)),
                };
                let sessions = match list_sessions(&list_runtime.config_path, request).await {
                    Ok(sessions) => sessions,
                    Err(error) => return responder.respond_with_error(internal_error(error)),
                };
                match v1::ListSessionsResponse::try_from(v2::ListSessionsResponse::new(sessions)) {
                    Ok(response) => responder.respond(response),
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::LoadSessionRequest,
                        responder: Responder<v1::LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let runtime = load_runtime.clone();
                cx.clone().spawn(async move {
                    let request = match v2::ResumeSessionRequest::try_from(request) {
                        Ok(request) => request,
                        Err(error) => {
                            responder.respond_with_error(invalid_params(error))?;
                            return Ok(());
                        }
                    };
                    run_load(runtime, request, responder, cx).await;
                    Ok(())
                })?;
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::ResumeSessionRequest,
                        responder: Responder<v1::ResumeSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let runtime = resume_runtime.clone();
                cx.clone().spawn(async move {
                    let request = match v2::ResumeSessionRequest::try_from(request) {
                        Ok(request) => request,
                        Err(error) => {
                            responder.respond_with_error(invalid_params(error))?;
                            return Ok(());
                        }
                    };
                    run_resume(runtime, request, responder, cx).await;
                    Ok(())
                })?;
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::CloseSessionRequest,
                        responder: Responder<v1::CloseSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                match close_session(&close_runtime, request.session_id.to_string()).await {
                    Ok(()) => responder.respond(v1::CloseSessionResponse::new()),
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::DeleteSessionRequest,
                        responder: Responder<v1::DeleteSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                match delete_session(&delete_runtime, request.session_id.to_string()).await {
                    Ok(()) => responder.respond(v1::DeleteSessionResponse::new()),
                    Err(error) => responder.respond_with_error(internal_error(error)),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::PromptRequest,
                        responder: Responder<v1::PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let runtime = prompt_runtime.clone();
                runtime
                    .pending_cancels
                    .consume(&request.session_id.to_string());
                cx.clone().spawn(async move {
                    run_prompt(runtime, request, responder, cx).await;
                    Ok(())
                })?;
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v1::SetSessionConfigOptionRequest,
                        responder: Responder<v1::SetSessionConfigOptionResponse>,
                        cx: ConnectionTo<Client>| {
                let request = match v2::SetSessionConfigOptionRequest::try_from(request) {
                    Ok(request) => request,
                    Err(error) => return responder.respond_with_error(invalid_params(error)),
                };
                set_config_option(&set_runtime, request, responder, cx).await
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: v1::CancelNotification, _cx: ConnectionTo<Client>| {
                defer_cancel_v1(&cancel_runtime, notification.session_id.to_string()).await;
                Ok(())
            },
            on_receive_notification!(),
        );

    connect_until_eof(agent, transport, eof)
        .await
        .map_err(|error| anyhow::anyhow!("ACP v1 connection failed: {error}"))
}

fn agent_capabilities() -> v2::AgentCapabilities {
    v2::AgentCapabilities::new().session(
        v2::SessionCapabilities::new()
            .prompt(
                v2::PromptCapabilities::new()
                    .image(v2::PromptImageCapabilities::new())
                    .embedded_context(v2::PromptEmbeddedContextCapabilities::new()),
            )
            .fork(v2::SessionForkCapabilities::new())
            .delete(v2::SessionDeleteCapabilities::new()),
    )
}

async fn list_sessions(
    config_path: &Path,
    request: v2::ListSessionsRequest,
) -> Result<Vec<v2::SessionInfo>> {
    if request.cursor.is_some() {
        return Ok(Vec::new());
    }
    let value = ipc::request(config_path, "session.list", json!({"all": true})).await?;
    let records: Vec<ipc_schema::SessionRecord> = serde_json::from_value(value).unwrap_or_default();
    Ok(records
        .into_iter()
        .filter(|record| {
            request
                .cwd
                .as_ref()
                .is_none_or(|cwd| record.info.cwd == AsRef::<Path>::as_ref(cwd))
        })
        .map(|record| {
            v2::SessionInfo::new(v2::SessionId::new(record.info.id.as_str()), record.info.cwd)
                .title(record.info.title)
                .updated_at(timestamp_rfc3339(record.info.updated_at_ms))
        })
        .collect())
}

async fn run_load(
    runtime: AcpRuntime,
    request: v2::ResumeSessionRequest,
    responder: Responder<v1::LoadSessionResponse>,
    cx: ConnectionTo<Client>,
) {
    let session_id = request.session_id.to_string();
    if let Err(error) = validate_resume(&runtime, &request).await {
        let _ = responder.respond_with_error(invalid_params(error));
        return;
    }
    let prepared = match prepare_observer(&runtime, &session_id, true).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let options = match session_config_options(&runtime.config_path, &session_id).await {
        Ok(options) => options,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let options = match options
        .into_iter()
        .map(v1::SessionConfigOption::try_from)
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(options) => options,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let connection = AcpConnection::new(AcpProtocol::V1, cx, runtime.compaction_supported.clone());
    if responder
        .respond(v1::LoadSessionResponse::new().config_options(options))
        .is_ok()
    {
        if let Err(error) = activate_observer(&runtime, &session_id, &connection, prepared).await {
            tracing::warn!(error = %format!("{error:#}"), "activate ACP v1 load observer failed");
            return;
        }
        send_available_commands(&connection, &session_id);
    }
}

async fn run_resume(
    runtime: AcpRuntime,
    request: v2::ResumeSessionRequest,
    responder: Responder<v1::ResumeSessionResponse>,
    cx: ConnectionTo<Client>,
) {
    let session_id = request.session_id.to_string();
    if let Err(error) = validate_resume(&runtime, &request).await {
        let _ = responder.respond_with_error(invalid_params(error));
        return;
    }
    let prepared = match prepare_observer(&runtime, &session_id, false).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let options = match session_config_options(&runtime.config_path, &session_id).await {
        Ok(options) => options,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let response = match v1::ResumeSessionResponse::try_from(
        v2::ResumeSessionResponse::new().config_options(options),
    ) {
        Ok(response) => response,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let connection = AcpConnection::new(AcpProtocol::V1, cx, runtime.compaction_supported.clone());
    if responder.respond(response).is_ok() {
        if activate_observer(&runtime, &session_id, &connection, prepared)
            .await
            .is_ok()
        {
            send_available_commands(&connection, &session_id);
        }
    }
}

async fn run_prompt(
    runtime: AcpRuntime,
    request: v1::PromptRequest,
    responder: Responder<v1::PromptResponse>,
    cx: ConnectionTo<Client>,
) {
    let request = match v2::PromptRequest::try_from(request) {
        Ok(request) => request,
        Err(error) => {
            let _ = responder.respond_with_error(invalid_params(error));
            return;
        }
    };
    let session_id = request.session_id.to_string();
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
    let prepared = match prepare_observer(&runtime, &session_id, false).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let connection = AcpConnection::new(AcpProtocol::V1, cx, runtime.compaction_supported.clone());
    let observer = match activate_observer(&runtime, &session_id, &connection, prepared).await {
        Ok(observer) => observer,
        Err(error) => {
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    let (sender, completion) = oneshot::channel();
    {
        let mut waiters = runtime.prompt_waiters.lock().await;
        if waiters.contains_key(&session_id) {
            let _ = responder.respond_with_error(invalid_params(
                "session already has an active ACP v1 prompt",
            ));
            return;
        }
        waiters.insert(session_id.clone(), sender);
    }
    let result = submit_prompt(&runtime, &session_id, &observer, command, content).await;
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            runtime.prompt_waiters.lock().await.remove(&session_id);
            let _ = responder.respond_with_error(internal_error(error));
            return;
        }
    };
    send_fork_result(&connection, &session_id, &value);
    if let Some(message_id) = value.get("message_id").and_then(Value::as_str) {
        let prompt = serde_json::to_value(request.prompt).unwrap_or_else(|_| Value::Array(vec![]));
        send_user_message(&connection, &session_id, message_id, &prompt);
    }
    if value.get("accepted").and_then(Value::as_bool) == Some(false) {
        runtime.prompt_waiters.lock().await.remove(&session_id);
        let _ = responder.respond(v1::PromptResponse::new(v1::StopReason::EndTurn));
        return;
    }
    match completion.await {
        Ok(completion) => match v1_stop_reason(completion) {
            Ok(reason) => {
                let _ = responder.respond(v1::PromptResponse::new(reason));
            }
            Err(error) => {
                let _ = responder.respond_with_error(internal_error(error));
            }
        },
        Err(_) => {
            let _ = responder.respond_with_error(internal_error("prompt completion was dropped"));
        }
    }
}

pub(super) fn v1_stop_reason(completion: PromptCompletion) -> Result<v1::StopReason> {
    let reason = completion.map_err(anyhow::Error::msg)?;
    v1::StopReason::try_from(reason).map_err(anyhow::Error::from)
}

async fn submit_prompt(
    runtime: &AcpRuntime,
    session_id: &str,
    observer: &SessionObserver,
    command: Option<SlashCommand>,
    content: MessageContent,
) -> Result<Value> {
    let (method, params) = match command {
        Some(SlashCommand::Compact) => (
            "session.compact",
            json!({"session_id": session_id, "endpoint_id": observer.endpoint_id}),
        ),
        Some(SlashCommand::Resume) => (
            "session.resume-turn",
            json!({"session_id": session_id, "endpoint_id": observer.endpoint_id}),
        ),
        Some(SlashCommand::Fork) => ("session.fork", json!({"session_id": session_id})),
        None => (
            "session.prompt",
            json!({
                "session_id": session_id,
                "endpoint_id": observer.endpoint_id,
                "message": content,
            }),
        ),
    };
    ipc::request(&runtime.config_path, method, params).await
}

async fn set_config_option(
    runtime: &AcpRuntime,
    request: v2::SetSessionConfigOptionRequest,
    responder: Responder<v1::SetSessionConfigOptionResponse>,
    cx: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let session_id = request.session_id.to_string();
    let config_id = request.config_id.to_string();
    let Some(value) = request.value.as_id().map(ToString::to_string) else {
        return responder.respond_with_error(invalid_params("session config value must be an id"));
    };
    let changed = match ipc::request(
        &runtime.config_path,
        "session.set_config_option",
        json!({"session_id": session_id, "config_id": config_id, "value": value}),
    )
    .await
    {
        Ok(changed) => changed,
        Err(error) => return responder.respond_with_error(internal_error(error)),
    };
    let options = match session_config_options(&runtime.config_path, &session_id).await {
        Ok(options) => options,
        Err(error) => return responder.respond_with_error(internal_error(error)),
    };
    let response = match v1::SetSessionConfigOptionResponse::try_from(
        v2::SetSessionConfigOptionResponse::new(options),
    ) {
        Ok(response) => response,
        Err(error) => return responder.respond_with_error(internal_error(error)),
    };
    let result = responder.respond(response);
    if result.is_ok()
        && config_id == "model"
        && !runtime.observers.lock().await.contains_key(&session_id)
        && let Some((used, size)) = snapshot_usage(&changed)
    {
        send_usage_update(
            &AcpConnection::new(AcpProtocol::V1, cx, runtime.compaction_supported.clone()),
            &session_id,
            used,
            size,
        );
    }
    result
}

#[cfg(test)]
mod compaction_capability_tests {
    use super::*;

    #[test]
    fn v1_requires_the_session_compaction_capability() {
        let unsupported: InitializeRequest = serde_json::from_value(json!({
            "protocolVersion": 1,
            "clientCapabilities": {"session": {}}
        }))
        .unwrap();
        assert!(!advertises_compaction(&unsupported));

        let supported: InitializeRequest = serde_json::from_value(json!({
            "protocolVersion": 1,
            "clientCapabilities": {"session": {"compaction": {}}}
        }))
        .unwrap();
        assert!(advertises_compaction(&supported));
    }
}

async fn close_session(runtime: &AcpRuntime, session_id: String) -> Result<()> {
    ipc::request(
        &runtime.config_path,
        "session.close",
        json!({"session_id": session_id}),
    )
    .await?;
    runtime.observers.lock().await.remove(&session_id);
    Ok(())
}

async fn delete_session(runtime: &AcpRuntime, session_id: String) -> Result<()> {
    ipc::request(
        &runtime.config_path,
        "session.delete",
        json!({"session_id": session_id}),
    )
    .await?;
    runtime.observers.lock().await.remove(&session_id);
    Ok(())
}
