use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use dwo_agent_service::{
    CompactionAccepted, ConfirmationDecision, EndpointId, PromptAccepted, SessionConfigUpdate,
    SessionId, SessionListItem, SessionSnapshot, SessionSubscription, TurnId,
};
use dwo_context::MessageContent;

mod attachments;
mod bridge;
mod feishu;
mod gateway;
mod manager;
mod permission;
mod qq;
mod render;
mod telegram;
mod weixin;

pub use gateway::{ChannelBindingProgress, ChannelGateway, ChannelPollParams};
pub use manager::{
    ChannelManager, FeishuBindProgress, QqBindProgress, TelegramBindProgress, WeixinLoginProgress,
};

pub const BIND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Default)]
pub struct SessionCreateRequest {
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub from: Option<SessionId>,
    pub project: Option<String>,
    pub section: Option<String>,
}

#[derive(Clone)]
pub struct ChannelSessionItem {
    pub session: SessionListItem,
    pub section: Option<String>,
    /// 会话当前有活跃回合；Inbox 视图用它分组「进行中 / 已完成」。
    pub working: bool,
}

#[async_trait]
pub trait ChannelHost: Send + Sync {
    fn profile_root_path(&self) -> &Path;
    fn channels(&self) -> Arc<ChannelManager>;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    Weixin,
    Telegram,
    Feishu,
    Qq,
}

impl ChannelKind {
    pub const ALL: [Self; 4] = [Self::Weixin, Self::Telegram, Self::Feishu, Self::Qq];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Weixin => "weixin",
            Self::Telegram => "telegram",
            Self::Feishu => "feishu",
            Self::Qq => "qq",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Weixin => "Weixin",
            Self::Telegram => "Telegram",
            Self::Feishu => "Feishu",
            Self::Qq => "QQ",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.as_str() == value)
    }
}
