//! Thin stdio handlers for Dwo extension requests.

use std::sync::Arc;

use agent_client_protocol::schema::LoadSessionResponse;
use agent_client_protocol::{Client, ConnectionTo, Responder};
use serde_json::Value;

use crate::agent::service::AgentService;
use crate::agent::session::SESSION_CLIENT_TRANSCRIPT_FILE;
use crate::automation::{
    AutomationNotificationRecord, AutomationNotifyChannel, automation_notification_text,
    record_automation_delivery, run_automation_job_once_with_leases_and_updates,
};
use crate::ingress::handler::{OutboundEmitter, handle_ingress_event};
use crate::protocol::acp::{mapper, notifications, transcript};
use crate::protocol::dwo::{
    self, DwoAutomationRecordDeliveryRequest, DwoAutomationRunJobRequest, DwoIngressChannel,
    DwoIngressHandleEventRequest, DwoIngressNotifyEventNotification, DwoOutboundAction,
    DwoOutboundActionNotification, DwoOutboundBody, DwoSessionContextRequest,
    DwoSessionLoadRequest, DwoSessionSetConfigOptionNotification, DwoWorkerPingRequest,
    DwoWorkerProfileRequest, DwoWorkerShutdownRequest,
};
use crate::tools::UpdateEmitter;

pub async fn worker_ping(
    _req: DwoWorkerPingRequest,
    responder: Responder<Value>,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    responder.respond(dwo::worker_ping_response())
}

pub async fn worker_profile(
    agent: Arc<AgentService>,
    _req: DwoWorkerProfileRequest,
    responder: Responder<Value>,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let snapshot = agent.profile_snapshot();
    responder.respond(dwo::worker_profile_response(
        &dwo::DwoWorkerProfileSnapshot {
            agent_id: snapshot.agent_id,
            name: snapshot.name,
            description: snapshot.description,
            agent_structure_dir: snapshot.agent_structure_dir,
            default_model_id: snapshot.default_model_id,
        },
    ))
}

pub async fn session_context(
    agent: Arc<AgentService>,
    req: DwoSessionContextRequest,
    responder: Responder<Value>,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let session_id = match dwo::normalize_session_context_request(req) {
        Ok(session_id) => session_id,
        Err(err) => return responder.respond_with_error(invalid_params(err)),
    };
    match agent.session_context_snapshot(&session_id).await {
        Ok(Some(snapshot)) => responder.respond(dwo::session_context_response(
            &dwo::DwoSessionContextSnapshot {
                session_id: snapshot.session_id,
                messages: snapshot.messages,
            },
        )),
        Ok(None) => {
            responder.respond_with_error(invalid_params(format!("unknown session `{session_id}`")))
        }
        Err(err) => responder.respond_with_error(invalid_params(err)),
    }
}

pub async fn session_load(
    agent: Arc<AgentService>,
    req: DwoSessionLoadRequest,
    responder: Responder<Value>,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.trim().to_string();
    if session_id.is_empty() {
        return responder
            .respond_with_error(invalid_params("_dwo/session/load requires sessionId"));
    }
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
            let mut replay_events = match transcript::transcript_replay_events(
                &session_id,
                &session.session_dir().join(SESSION_CLIENT_TRANSCRIPT_FILE),
            ) {
                Ok(events) => events,
                Err(err) => return responder.respond_with_error(internal_error(err)),
            };
            if let Some(event) = notifications::session_info_event(
                &session_id,
                snapshot.title.as_deref(),
                snapshot.updated_at.as_deref(),
            ) {
                replay_events.push(event);
            }
            match session.persisted_context_usage_snapshot() {
                Ok(Some(usage)) => {
                    if let Some(event) = notifications::context_usage_event(&session_id, usage) {
                        replay_events.push(event);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %format!("{err:#}"),
                        "failed to read persisted context usage for _dwo/session/load"
                    );
                }
            }
            match serde_json::to_value(response) {
                Ok(response) => {
                    responder.respond(dwo::session_load_response(response, replay_events))
                }
                Err(err) => responder.respond_with_error(internal_error(err)),
            }
        }
        Ok(None) => responder.respond_with_error(invalid_params("session not found")),
        Err(err) => responder.respond_with_error(internal_error(err)),
    }
}

pub async fn ingress_handle_event(
    agent: Arc<AgentService>,
    req: DwoIngressHandleEventRequest,
    responder: Responder<Value>,
    cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let emit_outbound = outbound_emitter(&cx);
    let emit_session_update = session_update_emitter(&cx);
    cx.spawn(async move {
        match handle_ingress_event(
            agent,
            req.event,
            Some(emit_outbound),
            Some(emit_session_update),
        )
        .await
        {
            Ok(result) => responder.respond(dwo::ingress_handle_event_response(
                result.session_id,
                result.actions,
            )),
            Err(err) => responder.respond_with_error(invalid_params(err)),
        }
    })?;
    Ok(())
}

pub async fn ingress_notify_event(
    agent: Arc<AgentService>,
    notif: DwoIngressNotifyEventNotification,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    if let Err(err) = handle_ingress_event(agent, notif.event, None, None).await {
        tracing::warn!(error = %format!("{err:#}"), "failed to handle ingress notification");
    }
    Ok(())
}

pub async fn session_set_config_option(
    agent: Arc<AgentService>,
    notif: DwoSessionSetConfigOptionNotification,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    if let Err(err) = agent
        .set_session_config_option(&notif.session_id, &notif.config_id, &notif.value)
        .await
    {
        tracing::warn!(
            session_id = %notif.session_id,
            config_id = %notif.config_id,
            error = %format!("{err:#}"),
            "failed to apply session config notification"
        );
    }
    Ok(())
}

pub async fn automation_run_job(
    agent: Arc<AgentService>,
    req: DwoAutomationRunJobRequest,
    responder: Responder<Value>,
    cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let job_id = match dwo::normalize_automation_run_job_request(req) {
        Ok(job_id) => job_id,
        Err(err) => return responder.respond_with_error(invalid_params(err)),
    };
    let emit_session_update = session_update_emitter(&cx);
    match run_automation_job_once_with_leases_and_updates(
        agent.clone(),
        agent.agent_structure_dir(),
        &job_id,
        agent.session_leases(),
        Some(emit_session_update),
    )
    .await
    {
        Ok((job, record)) => {
            let text = automation_notification_text(&job, &record);
            let notifications = job
                .notify
                .iter()
                .map(|notify| DwoOutboundAction {
                    channel: match notify.channel {
                        AutomationNotifyChannel::Weixin => DwoIngressChannel::Weixin,
                        AutomationNotifyChannel::Feishu => DwoIngressChannel::Feishu,
                    },
                    target: notify
                        .recipient
                        .as_ref()
                        .map(|recipient| recipient.id.clone())
                        .unwrap_or_default(),
                    body: DwoOutboundBody::Text { text: text.clone() },
                })
                .collect::<Vec<_>>();
            match serde_json::to_value(&record) {
                Ok(record) => {
                    responder.respond(dwo::automation_run_job_response(record, notifications))
                }
                Err(err) => responder.respond_with_error(internal_error(err)),
            }
        }
        Err(err) => responder.respond_with_error(internal_error(err)),
    }
}

pub async fn automation_record_delivery(
    agent: Arc<AgentService>,
    req: DwoAutomationRecordDeliveryRequest,
    responder: Responder<Value>,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    let req = match dwo::normalize_automation_record_delivery_request(req) {
        Ok(req) => req,
        Err(err) => return responder.respond_with_error(invalid_params(err)),
    };
    let mut notifications = Vec::new();
    for value in req.notifications {
        match serde_json::from_value::<AutomationNotificationRecord>(value) {
            Ok(record) => notifications.push(record),
            Err(err) => return responder.respond_with_error(invalid_params(err)),
        }
    }
    match record_automation_delivery(
        agent,
        &req.job_id,
        &req.run_id,
        &req.session_id,
        notifications,
    )
    .await
    {
        Ok(()) => responder.respond(dwo::automation_record_delivery_response()),
        Err(err) => responder.respond_with_error(internal_error(err)),
    }
}

pub async fn worker_shutdown(
    _req: DwoWorkerShutdownRequest,
    responder: Responder<Value>,
    _cx: ConnectionTo<Client>,
) -> std::result::Result<(), agent_client_protocol::Error> {
    responder.respond(dwo::worker_shutdown_response())
}

fn invalid_params(err: impl std::fmt::Display) -> agent_client_protocol::schema::Error {
    agent_client_protocol::schema::Error::invalid_params().data(format!("{err:#}"))
}

fn internal_error(err: impl std::fmt::Display) -> agent_client_protocol::schema::Error {
    agent_client_protocol::schema::Error::internal_error().data(format!("{err:#}"))
}

fn outbound_emitter(cx: &ConnectionTo<Client>) -> OutboundEmitter {
    let cx_for_emit = cx.clone();
    Arc::new(move |action: DwoOutboundAction| {
        let cx = cx_for_emit.clone();
        Box::pin(async move {
            let _ = cx.send_notification_to(Client, DwoOutboundActionNotification { action });
            Ok(())
        })
    })
}

fn session_update_emitter(cx: &ConnectionTo<Client>) -> UpdateEmitter {
    let cx_for_emit = cx.clone();
    Arc::new(move |session_id: String, update| {
        let cx = cx_for_emit.clone();
        Box::pin(async move {
            notifications::emit_session_update(&cx, &session_id, &update);
            Ok(())
        })
    })
}
