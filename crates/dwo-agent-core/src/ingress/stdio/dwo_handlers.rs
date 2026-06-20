//! Thin stdio handlers for Dwo extension requests.

use std::sync::Arc;

use agent_client_protocol::{Client, ConnectionTo, Responder};
use serde_json::Value;

use crate::agent::service::AgentService;
use crate::protocol::dwo::{
    self, DwoSessionContextRequest, DwoWorkerPingRequest, DwoWorkerProfileRequest,
    DwoWorkerShutdownRequest,
};

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
    responder.respond(dwo::worker_profile_response(&snapshot))
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
        Ok(Some(snapshot)) => responder.respond(dwo::session_context_response(&snapshot)),
        Ok(None) => {
            responder.respond_with_error(invalid_params(format!("unknown session `{session_id}`")))
        }
        Err(err) => responder.respond_with_error(invalid_params(err)),
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
