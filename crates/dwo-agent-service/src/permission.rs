use std::sync::Arc;

use dwo_tools::{ConfirmationDecision, ConfirmationHandler};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::TurnId;
use crate::error::SessionServiceError;
use crate::events::PendingPermission;
use crate::session::ActorEvent;

#[derive(Clone)]
pub(crate) struct PermissionRequester {
    turn_id: TurnId,
    cancellation: CancellationToken,
    gate: Arc<Mutex<()>>,
    requests: mpsc::UnboundedSender<ActorEvent>,
}

impl PermissionRequester {
    pub(crate) fn new(
        turn_id: TurnId,
        cancellation: CancellationToken,
        requests: mpsc::UnboundedSender<ActorEvent>,
    ) -> Self {
        Self {
            turn_id,
            cancellation,
            gate: Arc::new(Mutex::new(())),
            requests,
        }
    }

    pub(crate) fn confirmation_handler(&self) -> ConfirmationHandler {
        let requester = self.clone();
        Arc::new(move |request| {
            let requester = requester.clone();
            Box::pin(async move {
                let gate = requester.gate.clone();
                let _gate = tokio::select! {
                    _ = requester.cancellation.cancelled() => {
                        return rejected("turn cancelled");
                    }
                    guard = gate.lock_owned() => guard,
                };
                if requester.cancellation.is_cancelled() {
                    return rejected("turn cancelled");
                }

                let permission = PendingPermission {
                    request_id: format!("permission-{}", &Uuid::new_v4().simple().to_string()[..8]),
                    tool_call_id: request.tool_call_id,
                    tool_name: request.tool_name,
                };
                let (response, decision) = oneshot::channel();
                if requester
                    .requests
                    .send(ActorEvent::PermissionRequested(PermissionRequestEnvelope {
                        turn_id: requester.turn_id.clone(),
                        permission,
                        response,
                    }))
                    .is_err()
                {
                    return rejected("session closed while requesting permission");
                }

                tokio::select! {
                    _ = requester.cancellation.cancelled() => rejected("turn cancelled"),
                    decision = decision => decision.unwrap_or_else(|_| rejected("permission request was dropped")),
                }
            })
        })
    }
}

pub(crate) struct PermissionRequestEnvelope {
    pub(crate) turn_id: TurnId,
    pub(crate) permission: PendingPermission,
    response: oneshot::Sender<ConfirmationDecision>,
}

impl PermissionRequestEnvelope {
    pub(crate) fn reject(self, reason: impl Into<String>) {
        let _ = self.response.send(rejected(reason));
    }
}

#[derive(Default)]
pub(crate) struct PermissionState {
    pending: Option<PendingPermissionState>,
}

impl PermissionState {
    pub(crate) fn register(&mut self, request: PermissionRequestEnvelope) {
        self.reject("permission request was replaced");
        self.pending = Some(PendingPermissionState {
            turn_id: request.turn_id,
            view: request.permission,
            response: request.response,
        });
    }

    pub(crate) fn respond(
        &mut self,
        request_id: &str,
        decision: ConfirmationDecision,
    ) -> Result<ResolvedPermission, SessionServiceError> {
        let Some(pending) = self.pending.take() else {
            return Err(SessionServiceError::PermissionNotFound(
                request_id.to_string(),
            ));
        };
        if pending.view.request_id != request_id {
            self.pending = Some(pending);
            return Err(SessionServiceError::PermissionNotFound(
                request_id.to_string(),
            ));
        }
        let _ = pending.response.send(decision);
        Ok(ResolvedPermission {
            turn_id: pending.turn_id,
            request_id: pending.view.request_id,
        })
    }

    pub(crate) fn reject(&mut self, reason: impl Into<String>) {
        if let Some(pending) = self.pending.take() {
            let _ = pending.response.send(rejected(reason));
        }
    }

    pub(crate) fn snapshot(&self) -> Option<PendingPermission> {
        self.pending.as_ref().map(|pending| pending.view.clone())
    }
}

struct PendingPermissionState {
    turn_id: TurnId,
    view: PendingPermission,
    response: oneshot::Sender<ConfirmationDecision>,
}

pub(crate) struct ResolvedPermission {
    pub(crate) turn_id: TurnId,
    pub(crate) request_id: String,
}

fn rejected(reason: impl Into<String>) -> ConfirmationDecision {
    ConfirmationDecision {
        allowed: false,
        reason: Some(reason.into()),
    }
}
