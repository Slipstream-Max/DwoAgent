use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use dwo_agent_service::{
    CompactionAccepted, ConfirmationDecision, EndpointId, MessageContent, PromptAccepted,
    SessionConfigUpdate, SessionId, SessionListItem, SessionSnapshot, SessionSubscription, TurnId,
};

use crate::types::{
    ChannelBindingProgress, ChannelKind, ChannelPollParams, ChannelSessionItem, ChannelSummary,
    SessionCreateRequest,
};

/// Session-facing surface provided by the host to channel adapters.
#[async_trait]
pub trait ChannelHost: Send + Sync {
    fn profile_root_path(&self) -> &Path;

    async fn list_sessions(
        &self,
        all: bool,
        caller: Option<&SessionId>,
    ) -> Result<Vec<SessionListItem>>;
    async fn list_channel_sessions(&self, project: Option<&str>)
    -> Result<Vec<ChannelSessionItem>>;
    /// 可用技能快照（`name, description`），按扫描顺序。
    async fn list_skills(&self, session: Option<&SessionId>) -> Result<Vec<(String, String)>>;
    async fn session_options(&self, id: &SessionId) -> Result<dwo_protocol::SessionOptions>;
    async fn create_session(&self, request: SessionCreateRequest) -> Result<SessionSnapshot>;
    async fn subscribe_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription>;
    async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot>;
    async fn archive_session(&self, id: &SessionId) -> Result<()>;
    async fn cancel_session(&self, id: &SessionId, expected_turn_id: Option<TurnId>) -> Result<()>;
    async fn compact_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<CompactionAccepted>;
    async fn prompt_internal(&self, id: &SessionId, content: MessageContent) -> Result<TurnId>;
    async fn set_session_config(&self, id: &SessionId, update: SessionConfigUpdate) -> Result<()>;
    async fn resolve_session_permission(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<()>;
    async fn prompt_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted>;
}

/// Read-side channel state used by the host RPC surface.
#[async_trait]
pub trait ChannelControl: Send + Sync {
    async fn list(&self) -> Result<Vec<ChannelSummary>>;
    async fn summary(&self, channel: ChannelKind) -> Result<ChannelSummary>;
    async fn bound_target(&self, channel: ChannelKind) -> Result<String>;
}

/// Runtime channel control used by the host: binding flows, lifecycle, and
/// outbound messages.
#[async_trait]
pub trait ChannelGatewayApi: Send + Sync {
    async fn begin_bind(&self, channel: ChannelKind, host: Arc<dyn ChannelHost>) -> Result<Value>;
    async fn poll_bind(
        &self,
        channel: ChannelKind,
        host: Arc<dyn ChannelHost>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress>;
    async fn unbind(&self, channel: ChannelKind, host: Arc<dyn ChannelHost>) -> Result<bool>;
    async fn start_all(&self, host: Arc<dyn ChannelHost>);
    async fn stop_all(&self);
    async fn send_message(&self, channel: ChannelKind, text: &str) -> Result<()>;
    async fn send_file(&self, channel: ChannelKind, path: &Path) -> Result<()>;
}

/// A manager/gateway pair created together for one generation of channel
/// configuration.
pub struct ChannelBundle {
    pub control: Arc<dyn ChannelControl>,
    pub gateway: Arc<dyn ChannelGatewayApi>,
}

/// Creates channel managers and gateways so that the host never needs to know
/// the concrete implementations.
#[async_trait]
pub trait ChannelFactory: Send + Sync {
    async fn create(
        &self,
        profile_root: &Path,
        channels: &BTreeMap<String, serde_yaml::Value>,
    ) -> Result<ChannelBundle>;
}
