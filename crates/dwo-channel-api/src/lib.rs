//! Shared channel interface: neutral types and traits used between the host
//! and concrete channel implementations.
//!
//! `dwo-host` and `dwo-cli` depend on this crate only. The concrete adapters
//! and their platform SDKs live in `dwo-channels` and are wired in through
//! [`ChannelFactory`].

mod traits;
mod types;

pub use traits::{ChannelBundle, ChannelControl, ChannelFactory, ChannelGatewayApi, ChannelHost};
pub use types::{
    BIND_POLL_INTERVAL, ChannelBindingProgress, ChannelKind, ChannelPollParams, ChannelSessionItem,
    ChannelSummary, FeishuBindProgress, QqBindProgress, SessionCreateRequest, TelegramBindProgress,
    WeixinLoginProgress,
};
