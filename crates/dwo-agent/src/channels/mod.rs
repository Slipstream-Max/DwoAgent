mod attachments;
mod bridge;
mod command;
mod feishu;
mod hub;
mod manager;
mod render;
mod telegram;
mod websocket;
mod weixin;

pub(crate) use hub::ChannelHub;
pub(crate) use manager::{
    ChannelManager, FeishuBindProgress, TelegramBindProgress, WeixinLoginProgress, wait_before_poll,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChannelKind {
    Weixin,
    Telegram,
    Feishu,
    Websocket,
}

impl ChannelKind {
    pub(crate) const ALL: [Self; 4] = [Self::Weixin, Self::Telegram, Self::Feishu, Self::Websocket];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Weixin => "weixin",
            Self::Telegram => "telegram",
            Self::Feishu => "feishu",
            Self::Websocket => "websocket",
        }
    }

    pub(crate) const fn display_name(self) -> &'static str {
        match self {
            Self::Weixin => "Weixin",
            Self::Telegram => "Telegram",
            Self::Feishu => "Feishu",
            Self::Websocket => "WebSocket",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.as_str() == value)
    }
}
