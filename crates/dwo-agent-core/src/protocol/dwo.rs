//! Dwo JSON-RPC extension protocol.

use std::path::PathBuf;

use agent_client_protocol::JsonRpcRequest;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DwoChannelCommand {
    Help,
    New,
    List,
    Switch {
        session_id: String,
    },
    Back,
    Where,
    Approve {
        confirmation_id: String,
    },
    Deny {
        confirmation_id: String,
        reason: Option<String>,
    },
    Cancel,
    Usage(String),
}

pub fn parse_channel_command(text: &str) -> Option<DwoChannelCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let command = parts.next().unwrap_or_default();
    match command {
        "/help" => Some(DwoChannelCommand::Help),
        "/new" => Some(DwoChannelCommand::New),
        "/list" => Some(DwoChannelCommand::List),
        "/switch" => match parts.next() {
            Some(session_id) => Some(DwoChannelCommand::Switch {
                session_id: session_id.to_string(),
            }),
            None => Some(DwoChannelCommand::Usage(
                "用法：/switch <session_id>".to_string(),
            )),
        },
        "/back" => Some(DwoChannelCommand::Back),
        "/where" => Some(DwoChannelCommand::Where),
        "/approve" => match parts.next() {
            Some(confirmation_id) => Some(DwoChannelCommand::Approve {
                confirmation_id: confirmation_id.to_string(),
            }),
            None => Some(DwoChannelCommand::Usage(
                "用法：/approve <confirmation_id>".to_string(),
            )),
        },
        "/deny" => match parts.next() {
            Some(confirmation_id) => {
                let reason = parts.collect::<Vec<_>>().join(" ");
                Some(DwoChannelCommand::Deny {
                    confirmation_id: confirmation_id.to_string(),
                    reason: if reason.trim().is_empty() {
                        None
                    } else {
                        Some(reason)
                    },
                })
            }
            None => Some(DwoChannelCommand::Usage(
                "用法：/deny <confirmation_id> [reason]".to_string(),
            )),
        },
        "/cancel" => Some(DwoChannelCommand::Cancel),
        _ => None,
    }
}

pub fn channel_command_help() -> &'static str {
    "可用命令：\n/new 创建并切换到新 session\n/list 查看最近 session\n/switch <session_id> 切换 session\n/back 返回默认 session\n/where 查看当前 session\n/cancel 取消当前 session 运行\n/approve <confirmation_id> 批准工具调用\n/deny <confirmation_id> [reason] 拒绝工具调用并给出原因\n/help 查看帮助"
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_dwo/worker/ping", response = serde_json::Value)]
pub struct DwoWorkerPingRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_dwo/worker/profile", response = serde_json::Value)]
pub struct DwoWorkerProfileRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_dwo/worker/shutdown", response = serde_json::Value)]
pub struct DwoWorkerShutdownRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_dwo/session/context", response = serde_json::Value)]
#[serde(deny_unknown_fields)]
pub struct DwoSessionContextRequest {
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct DwoWorkerProfileSnapshot {
    pub agent_id: String,
    pub name: String,
    pub description: String,
    pub agent_structure_dir: PathBuf,
    pub default_model_id: String,
}

#[derive(Debug, Clone)]
pub struct DwoSessionContextSnapshot {
    pub session_id: String,
    pub messages: Vec<Value>,
}

pub fn normalize_session_context_request(req: DwoSessionContextRequest) -> Result<String> {
    let session_id = req.session_id.trim().to_string();
    if session_id.is_empty() {
        bail!("_dwo/session/context requires non-empty session_id");
    }
    Ok(session_id)
}

pub fn worker_ping_response() -> Value {
    json!({ "ok": true })
}

pub fn worker_profile_response(snapshot: &DwoWorkerProfileSnapshot) -> Value {
    json!({
        "agent_id": snapshot.agent_id,
        "name": snapshot.name,
        "description": snapshot.description,
        "agent_structure_dir": snapshot.agent_structure_dir,
        "default_model_id": snapshot.default_model_id,
    })
}

pub fn session_context_response(snapshot: &DwoSessionContextSnapshot) -> Value {
    json!({
        "session_id": snapshot.session_id,
        "messages": snapshot.messages,
    })
}

pub fn worker_shutdown_response() -> Value {
    json!({ "ok": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_command_parser_handles_known_commands() {
        assert_eq!(parse_channel_command("hello"), None);
        assert_eq!(
            parse_channel_command("/help"),
            Some(DwoChannelCommand::Help)
        );
        assert_eq!(parse_channel_command("/new"), Some(DwoChannelCommand::New));
        assert_eq!(
            parse_channel_command("/cancel"),
            Some(DwoChannelCommand::Cancel)
        );
        assert_eq!(
            parse_channel_command("/approve c1"),
            Some(DwoChannelCommand::Approve {
                confirmation_id: "c1".to_string()
            })
        );
        assert_eq!(
            parse_channel_command("/switch s1"),
            Some(DwoChannelCommand::Switch {
                session_id: "s1".to_string()
            })
        );
        assert_eq!(
            parse_channel_command("/deny c1"),
            Some(DwoChannelCommand::Deny {
                confirmation_id: "c1".to_string(),
                reason: None
            })
        );
        assert_eq!(
            parse_channel_command("/deny c1 use read-only command"),
            Some(DwoChannelCommand::Deny {
                confirmation_id: "c1".to_string(),
                reason: Some("use read-only command".to_string())
            })
        );
    }

    #[test]
    fn channel_command_parser_reports_usage() {
        assert_eq!(
            parse_channel_command("/switch"),
            Some(DwoChannelCommand::Usage(
                "用法：/switch <session_id>".to_string()
            ))
        );
        assert_eq!(
            parse_channel_command("/deny"),
            Some(DwoChannelCommand::Usage(
                "用法：/deny <confirmation_id> [reason]".to_string()
            ))
        );
        assert_eq!(
            parse_channel_command("/approve"),
            Some(DwoChannelCommand::Usage(
                "用法：/approve <confirmation_id>".to_string()
            ))
        );
    }
}
