//! Stdio JSON-RPC ingress for ACP-compatible and Dwo extension messages.

mod acp_handlers;
mod dwo_handlers;
pub mod rpc_host;

pub use rpc_host::run_rpc_stdio;
