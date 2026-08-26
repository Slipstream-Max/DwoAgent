#![doc = include_str!("../README.md")]

pub mod automation;
mod host;
pub mod logging;

pub use dwo_agent_service::SessionId;
pub use host::events::{EventReadResult, HostEvent};
pub use host::{Host, HostSessionOptions, WebsocketRuntime, profile_root};
