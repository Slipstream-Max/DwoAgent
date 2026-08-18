#![doc = include_str!("../README.md")]

pub mod automation;
mod host;
pub mod logging;

pub use host::events::{EventReadResult, HostEvent};
pub use host::{Host, WebsocketRuntime, profile_root};
