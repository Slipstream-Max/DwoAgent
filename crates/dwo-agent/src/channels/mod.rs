mod attachments;
mod bridge;
mod feishu;
mod gateway;
mod manager;
mod qq;
mod render;
mod telegram;
mod websocket;
mod weixin;

pub(crate) use gateway::{ChannelGateway, ChannelPollParams};
pub(crate) use manager::{
    ChannelManager, FeishuBindProgress, QqBindProgress, TelegramBindProgress, WeixinLoginProgress,
};

pub(crate) const BIND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChannelKind {
    Weixin,
    Telegram,
    Feishu,
    Qq,
    Websocket,
}

impl ChannelKind {
    pub(crate) const ALL: [Self; 5] = [
        Self::Weixin,
        Self::Telegram,
        Self::Feishu,
        Self::Qq,
        Self::Websocket,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Weixin => "weixin",
            Self::Telegram => "telegram",
            Self::Feishu => "feishu",
            Self::Qq => "qq",
            Self::Websocket => "websocket",
        }
    }

    pub(crate) const fn display_name(self) -> &'static str {
        match self {
            Self::Weixin => "Weixin",
            Self::Telegram => "Telegram",
            Self::Feishu => "Feishu",
            Self::Qq => "QQ",
            Self::Websocket => "WebSocket",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.as_str() == value)
    }
}
