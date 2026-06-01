//! Weixin channel tool runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::subagent_tool_runtime::ToolExecutionContext;
use super::tool_run_manager::ChannelToolExecutor;
use crate::templates;
use crate::tools::tool_schemas_from_templates;

pub const WEIXIN_REPLY_MEDIA_TOOL: &str = "weixin_reply_media";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WeixinReplyMediaCall {
    path: String,
}

pub struct WeixinReplyMediaResult {
    pub message_id: String,
}

#[async_trait::async_trait]
pub trait WeixinToolBridge: Send + Sync {
    async fn reply_media(&self, path: &Path) -> Result<WeixinReplyMediaResult>;
}

pub struct WeixinToolExecutor {
    bridge: Arc<dyn WeixinToolBridge>,
    allowed_roots: Vec<PathBuf>,
}

impl WeixinToolExecutor {
    pub fn new(bridge: Arc<dyn WeixinToolBridge>, allowed_roots: Vec<PathBuf>) -> Self {
        Self {
            bridge,
            allowed_roots,
        }
    }
}

pub fn weixin_tool_schemas() -> Vec<Value> {
    tool_schemas_from_templates(&[templates::channel::weixin::TOOL_SCHEMA])
}

pub fn has_weixin_reply_media_tool(schemas: &[Value]) -> bool {
    schemas.iter().any(|schema| {
        schema
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            == Some(WEIXIN_REPLY_MEDIA_TOOL)
    })
}

#[async_trait::async_trait]
impl ChannelToolExecutor for WeixinToolExecutor {
    fn handles_tool(&self, name: &str) -> bool {
        name == WEIXIN_REPLY_MEDIA_TOOL
    }

    async fn execute_channel_tool(
        &self,
        name: &str,
        args: &Map<String, Value>,
        _context: Option<&ToolExecutionContext>,
    ) -> Result<Value> {
        if name != WEIXIN_REPLY_MEDIA_TOOL {
            bail!("Unknown Weixin tool: {name}");
        }

        let call: WeixinReplyMediaCall = serde_json::from_value(Value::Object(args.clone()))?;
        let raw_path = call.path.trim();
        if raw_path.is_empty() {
            bail!("path is required");
        }

        let path = resolve_allowed_file_path(raw_path, &self.allowed_roots)?;
        let result = self.bridge.reply_media(&path).await?;
        Ok(json!({
            "status": "ok",
            "done": true,
            "message_id": result.message_id,
            "path": path.to_string_lossy(),
        }))
    }
}

fn resolve_allowed_file_path(raw_path: &str, allowed_roots: &[PathBuf]) -> Result<PathBuf> {
    let raw = PathBuf::from(raw_path);
    let mut candidates = Vec::new();
    if raw.is_absolute() {
        candidates.push(raw.clone());
    } else {
        for root in allowed_roots {
            candidates.push(root.join(&raw));
        }
    }

    for candidate in candidates {
        let Ok(resolved) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if !resolved.is_file() {
            continue;
        }
        for root in allowed_roots {
            let allowed_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&allowed_root) {
                return Ok(resolved);
            }
        }
    }

    bail!("File path is not readable inside the allowed Weixin roots: {raw_path}");
}
