//! Feishu channel tool runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::subagent_tool_runtime::ToolExecutionContext;
use super::tool_run_manager::ChannelToolExecutor;
use crate::templates;
use crate::tools::tool_schemas_from_templates;

pub const FEISHU_REPLY_MEDIA_TOOL: &str = "feishu_reply_media";
pub const FEISHU_REPLY_CARD_TOOL: &str = "feishu_reply_card";

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FeishuReplyMediaKind {
    Auto,
    Image,
    File,
}

impl Default for FeishuReplyMediaKind {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeishuReplyMediaCall {
    path: String,
    #[serde(default)]
    kind: FeishuReplyMediaKind,
    #[serde(default)]
    file_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeishuReplyCardCall {
    card: Value,
}

pub struct FeishuReplyMediaResult {
    pub message_id: String,
    pub resource_key: String,
    pub msg_type: String,
}

pub struct FeishuReplyCardResult {
    pub message_id: String,
}

#[async_trait::async_trait]
pub trait FeishuToolBridge: Send + Sync {
    async fn reply_media(
        &self,
        path: &Path,
        kind: FeishuReplyMediaKind,
        file_type: Option<&str>,
    ) -> Result<FeishuReplyMediaResult>;

    async fn reply_card(&self, card: Value) -> Result<FeishuReplyCardResult>;
}

pub struct FeishuToolExecutor {
    bridge: Arc<dyn FeishuToolBridge>,
    allowed_roots: Vec<PathBuf>,
    media_output: bool,
    card_output: bool,
}

impl FeishuToolExecutor {
    pub fn new(
        bridge: Arc<dyn FeishuToolBridge>,
        allowed_roots: Vec<PathBuf>,
        media_output: bool,
        card_output: bool,
    ) -> Self {
        Self {
            bridge,
            allowed_roots,
            media_output,
            card_output,
        }
    }
}

pub fn feishu_tool_schemas(media_output: bool, card_output: bool) -> Vec<Value> {
    let mut sources = Vec::new();
    if media_output {
        sources.push(templates::channel::feishu::MEDIA_TOOL_SCHEMA);
    }
    if card_output {
        sources.push(templates::channel::feishu::CARD_TOOL_SCHEMA);
    }
    tool_schemas_from_templates(&sources)
}

#[async_trait::async_trait]
impl ChannelToolExecutor for FeishuToolExecutor {
    fn handles_tool(&self, name: &str) -> bool {
        (self.media_output && name == FEISHU_REPLY_MEDIA_TOOL)
            || (self.card_output && name == FEISHU_REPLY_CARD_TOOL)
    }

    async fn execute_channel_tool(
        &self,
        name: &str,
        args: &Map<String, Value>,
        _context: Option<&ToolExecutionContext>,
    ) -> Result<Value> {
        match name {
            FEISHU_REPLY_MEDIA_TOOL => {
                let call: FeishuReplyMediaCall =
                    serde_json::from_value(Value::Object(args.clone()))?;
                let raw_path = call.path.trim();
                if raw_path.is_empty() {
                    bail!("path is required");
                }
                let path = resolve_allowed_file_path(raw_path, &self.allowed_roots)?;
                let file_type = call
                    .file_type
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let result = self.bridge.reply_media(&path, call.kind, file_type).await?;
                Ok(json!({
                    "status": "ok",
                    "done": true,
                    "message_id": result.message_id,
                    "resource_key": result.resource_key,
                    "msg_type": result.msg_type,
                    "path": path.to_string_lossy(),
                }))
            }
            FEISHU_REPLY_CARD_TOOL => {
                let call: FeishuReplyCardCall =
                    serde_json::from_value(Value::Object(args.clone()))?;
                if !call.card.is_object() {
                    bail!("card must be a JSON object");
                }
                let result = self.bridge.reply_card(call.card).await?;
                Ok(json!({
                    "status": "ok",
                    "done": true,
                    "message_id": result.message_id,
                    "msg_type": "interactive",
                }))
            }
            other => bail!("Unknown Feishu tool: {other}"),
        }
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

    bail!("File path is not readable inside the allowed Feishu roots: {raw_path}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feishu_tool_schemas_follow_output_flags() {
        let schemas = feishu_tool_schemas(true, false);
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();
        assert_eq!(names, vec![FEISHU_REPLY_MEDIA_TOOL]);

        let schemas = feishu_tool_schemas(false, true);
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();
        assert_eq!(names, vec![FEISHU_REPLY_CARD_TOOL]);
    }
}
