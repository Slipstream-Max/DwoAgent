use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use dwo_agent_service::{
    ConfirmationDecision, EndpointId, PromptAccepted, SessionConfigUpdate, SessionId,
    SessionRecord, SessionSnapshot, SessionSubscription, TurnId,
};
use dwo_channels::{ChannelHost, ChannelManager};
use dwo_context::MessageContent;

use super::Host;

#[async_trait::async_trait]
impl ChannelHost for Host {
    fn profile_root_path(&self) -> &Path {
        Host::profile_root_path(self)
    }

    fn channels(&self) -> Arc<ChannelManager> {
        Host::channels(self)
    }

    async fn list_sessions(
        &self,
        all: bool,
        caller: Option<&SessionId>,
    ) -> Result<Vec<SessionRecord>> {
        Host::list_sessions(self, all, caller).await
    }

    async fn setup_session(
        &self,
        title: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<SessionSnapshot> {
        Host::setup_session(self, title, cwd).await
    }

    async fn fork_session(&self, source_id: &SessionId) -> Result<SessionSnapshot> {
        Host::fork_session(self, source_id).await
    }

    async fn subscribe_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription> {
        Host::subscribe_session(self, id, endpoint, checkpoint_cursor).await
    }

    async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot> {
        Host::session_snapshot(self, id).await
    }

    async fn delete_session(&self, id: &SessionId) -> Result<()> {
        Host::delete_session(self, id).await
    }

    async fn cancel_session(&self, id: &SessionId, expected_turn_id: Option<TurnId>) -> Result<()> {
        Host::cancel_session(self, id, expected_turn_id).await
    }

    async fn compact_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<PromptAccepted> {
        Host::compact_session(self, id, endpoint).await
    }

    async fn resume_session_turn(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<Option<PromptAccepted>> {
        Host::resume_session_turn(self, id, endpoint).await
    }

    async fn set_session_config(
        &self,
        id: &SessionId,
        update: SessionConfigUpdate,
    ) -> Result<SessionSnapshot> {
        Host::set_session_config(self, id, update).await
    }

    async fn resolve_session_permission(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<()> {
        Host::resolve_session_permission(self, id, endpoint, request_id, decision).await
    }

    async fn prompt_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted> {
        Host::prompt_session(self, id, endpoint, content).await
    }
}
