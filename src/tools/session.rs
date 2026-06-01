//! Shared tool session interface.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::HashSet;

/// Session capability flags. Mirror of the Python `Cap` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cap {
    Wait,
    Checkout,
    Send,
}

/// Arguments passed to [`ToolSession::start`] and the mirror wait/checkout
/// calls. Python uses `**kwargs` with loosely typed values; we keep a
/// `Map<String, Value>` to preserve that flexibility exactly.
pub type ToolArgs = Map<String, Value>;

#[async_trait]
pub trait ToolSession: Send + Sync {
    /// Stable session identifier assigned at construction time.
    fn session_id(&self) -> &str;

    /// Declared capabilities — empty by default.
    fn capabilities(&self) -> HashSet<Cap> {
        HashSet::new()
    }

    /// Start the session and return an immediate snapshot.
    async fn start(&mut self, args: &ToolArgs) -> Result<Value>;

    /// Terminate and clean up the session.
    async fn cancel(&mut self) -> Result<()>;

    /// Report whether the session has finished.
    fn is_done(&self) -> bool;

    /// Default list item shape — mirrors Python's `ToolSession.list_item`.
    fn list_item(&self) -> Value {
        let done = self.is_done();
        json!({
            "id": self.session_id(),
            "kind": "tool",
            "status": if done { "done" } else { "running" },
            "done": done,
        })
    }

    async fn wait(&mut self, _timeout_secs: f64, _args: &ToolArgs) -> Result<Value> {
        bail!("session does not support wait")
    }

    async fn checkout(&mut self, _args: &ToolArgs) -> Result<Value> {
        bail!("session does not support checkout")
    }

    async fn send(&mut self, _message: &str, _interrupt: bool) -> Result<Value> {
        bail!("session does not support send")
    }
}
