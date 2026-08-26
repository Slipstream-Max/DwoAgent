use crate::{SessionId, TurnId};

#[derive(Debug, thiserror::Error)]
pub enum SessionServiceError {
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),
    #[error("queued prompt was cancelled: {0}")]
    PromptCancelled(SessionId),
    #[error("session is busy: {0}")]
    SessionBusy(SessionId),
    #[error("turn is not active: {0}")]
    TurnNotActive(TurnId),
    #[error("permission request not found: {0}")]
    PermissionNotFound(String),
    #[error("session is closed: {0}")]
    SessionClosed(SessionId),
    #[error("session is being deleted: {0}")]
    SessionDeleting(SessionId),
    #[error("invalid session configuration: {0}")]
    InvalidConfig(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
