use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use dwo_agent_service::{
    CompactionAccepted, ConfirmationDecision, EndpointId, PromptAccepted, SessionConfigUpdate,
    SessionId, SessionListItem, SessionListQuery, SessionSnapshot, SessionSubscription, TurnId,
};
use dwo_channels::{ChannelHost, ChannelManager, SessionCreateRequest};
use dwo_context::MessageContent;

use super::{Host, HostSessionOptions};

#[async_trait::async_trait]
impl ChannelHost for Host {
    fn profile_root_path(&self) -> &Path {
        &self.profile_root
    }

    fn channels(&self) -> Arc<ChannelManager> {
        self.channels
            .read()
            .expect("channel manager lock poisoned")
            .clone()
    }

    async fn list_sessions(
        &self,
        all: bool,
        caller: Option<&SessionId>,
    ) -> Result<Vec<SessionListItem>> {
        let mut query = SessionListQuery::new(None, Some(500));
        if !all {
            query.parent_session_id = caller.cloned();
            query.roots_only = caller.is_none();
        }
        let mut sessions = Vec::new();
        loop {
            let page = self.service.list(query.clone()).await?;
            sessions.extend(page.sessions);
            if let Some(next) = page.next_cursor {
                query.cursor = Some(next);
            } else {
                return Ok(sessions);
            }
        }
    }

    async fn create_session(&self, request: SessionCreateRequest) -> Result<SessionSnapshot> {
        let id = self
            .create_session(HostSessionOptions {
                title: request.title,
                cwd: request.cwd,
                from: request.from,
                ..HostSessionOptions::default()
            })
            .await?;
        Ok(self.service.snapshot(&id).await?)
    }

    async fn subscribe_session(
        &self,
        id: &SessionId,
        _endpoint: EndpointId,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription> {
        Ok(self.service.subscribe(id, checkpoint_cursor).await?)
    }

    async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot> {
        Ok(self.service.snapshot(id).await?)
    }

    async fn delete_session(&self, id: &SessionId) -> Result<()> {
        Host::delete_session(self, id).await
    }

    async fn cancel_session(&self, id: &SessionId, expected_turn_id: Option<TurnId>) -> Result<()> {
        Ok(self.service.cancel(id, expected_turn_id).await?)
    }

    async fn compact_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<CompactionAccepted> {
        Ok(self.service.compact(id, endpoint).await?)
    }

    async fn prompt_internal(&self, id: &SessionId, content: MessageContent) -> Result<TurnId> {
        Ok(self.service.prompt_internal(id, content).await?)
    }

    async fn set_session_config(&self, id: &SessionId, update: SessionConfigUpdate) -> Result<()> {
        Ok(self.service.set_config(id, update).await?)
    }

    async fn resolve_session_permission(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<()> {
        Ok(self
            .service
            .respond_permission(id, endpoint, request_id, decision)
            .await?)
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
