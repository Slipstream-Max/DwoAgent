//! Dwo JSON-RPC extension protocol.

use std::path::PathBuf;

use agent_client_protocol::{JsonRpcNotification, JsonRpcRequest};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DwoChannelCommand {
    Help,
    New,
    List,
    Load { session_ref: String },
    SetHome { session_ref: String },
    Home,
    Where,
    Approve { confirmation_id: String },
    Deny { confirmation_id: String },
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
        "/load" => {
            let session_ref = parts.collect::<Vec<_>>().join(" ");
            if session_ref.is_empty() {
                Some(DwoChannelCommand::Usage(
                    "用法：/load <name_or_id>".to_string(),
                ))
            } else {
                Some(DwoChannelCommand::Load { session_ref })
            }
        }
        "/set-home" => {
            let session_ref = parts.collect::<Vec<_>>().join(" ");
            if session_ref.is_empty() {
                Some(DwoChannelCommand::Usage(
                    "用法：/set-home <name_or_id>".to_string(),
                ))
            } else {
                Some(DwoChannelCommand::SetHome { session_ref })
            }
        }
        "/home" => Some(DwoChannelCommand::Home),
        "/where" => Some(DwoChannelCommand::Where),
        "/approve" => match (parts.next(), parts.next()) {
            (Some(confirmation_id), None) => Some(DwoChannelCommand::Approve {
                confirmation_id: confirmation_id.to_string(),
            }),
            _ => Some(DwoChannelCommand::Usage(
                "用法：/approve <confirmation_id>".to_string(),
            )),
        },
        "/deny" => match (parts.next(), parts.next()) {
            (Some(confirmation_id), None) => Some(DwoChannelCommand::Deny {
                confirmation_id: confirmation_id.to_string(),
            }),
            _ => Some(DwoChannelCommand::Usage(
                "用法：/deny <confirmation_id>".to_string(),
            )),
        },
        "/cancel" => Some(DwoChannelCommand::Cancel),
        _ => Some(DwoChannelCommand::Usage(format!(
            "未知命令：{command}\n使用 /help 查看可用命令"
        ))),
    }
}

pub fn channel_command_help() -> &'static str {
    "可用命令：\n/new 创建并进入新话题\n/list 查看话题列表\n/load <name_or_id> 进入指定话题\n/set-home <name_or_id> 设置主话题\n/home 返回主话题\n/where 查看当前话题和主话题\n/cancel 取消当前话题运行\n/approve <confirmation_id> 批准工具调用\n/deny <confirmation_id> 拒绝工具调用\n/help 查看帮助"
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DwoSessionContextRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_dwo/session/load", response = serde_json::Value)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DwoSessionLoadRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_dwo/ingress/handle_event", response = serde_json::Value)]
#[serde(deny_unknown_fields)]
pub struct DwoIngressHandleEventRequest {
    pub event: DwoIngressEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_dwo/ingress/notify_event")]
#[serde(deny_unknown_fields)]
pub struct DwoIngressNotifyEventNotification {
    pub event: DwoIngressEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_dwo/outbound/action")]
#[serde(deny_unknown_fields)]
pub struct DwoOutboundActionNotification {
    pub action: DwoOutboundAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_dwo/session/set_config_option")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DwoSessionSetConfigOptionNotification {
    pub session_id: String,
    pub config_id: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_dwo/automation/run_job", response = serde_json::Value)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DwoAutomationRunJobRequest {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_dwo/automation/record_delivery", response = serde_json::Value)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DwoAutomationRecordDeliveryRequest {
    pub job_id: String,
    pub run_id: String,
    pub session_id: String,
    #[serde(default)]
    pub notifications: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DwoIngressChannel {
    Weixin,
    Feishu,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DwoIngressEvent {
    pub channel: DwoIngressChannel,
    pub source: DwoIngressSource,
    pub conversation: DwoIngressConversation,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attachments: Vec<DwoIngressAttachment>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DwoIngressSource {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DwoIngressConversation {
    pub id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub holder: Option<String>,
    #[serde(default)]
    pub state_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DwoIngressAttachment {
    pub path: PathBuf,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DwoOutboundAction {
    pub channel: DwoIngressChannel,
    pub target: String,
    #[serde(flatten)]
    pub body: DwoOutboundBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DwoOutboundBody {
    Text {
        text: String,
    },
    Media {
        path: PathBuf,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default, rename = "fileType")]
        file_type: Option<String>,
    },
    Card {
        card: Value,
    },
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
        bail!("_dwo/session/context requires non-empty sessionId");
    }
    Ok(session_id)
}

pub fn normalize_automation_run_job_request(req: DwoAutomationRunJobRequest) -> Result<String> {
    let job_id = req.job_id.trim().to_string();
    if job_id.is_empty() {
        bail!("_dwo/automation/run_job requires non-empty jobId");
    }
    Ok(job_id)
}

pub fn normalize_automation_record_delivery_request(
    req: DwoAutomationRecordDeliveryRequest,
) -> Result<DwoAutomationRecordDeliveryRequest> {
    let job_id = req.job_id.trim().to_string();
    let run_id = req.run_id.trim().to_string();
    let session_id = req.session_id.trim().to_string();
    if job_id.is_empty() {
        bail!("_dwo/automation/record_delivery requires non-empty jobId");
    }
    if run_id.is_empty() {
        bail!("_dwo/automation/record_delivery requires non-empty runId");
    }
    if session_id.is_empty() {
        bail!("_dwo/automation/record_delivery requires non-empty sessionId");
    }
    Ok(DwoAutomationRecordDeliveryRequest {
        job_id,
        run_id,
        session_id,
        notifications: req.notifications,
    })
}

pub fn worker_ping_response() -> Value {
    json!({ "ok": true })
}

pub fn worker_profile_response(snapshot: &DwoWorkerProfileSnapshot) -> Value {
    json!({
        "agentId": snapshot.agent_id,
        "name": snapshot.name,
        "description": snapshot.description,
        "agentStructureDir": snapshot.agent_structure_dir,
        "defaultModelId": snapshot.default_model_id,
    })
}

pub fn session_context_response(snapshot: &DwoSessionContextSnapshot) -> Value {
    json!({
        "sessionId": snapshot.session_id,
        "messages": snapshot.messages,
    })
}

pub fn session_load_response(response: Value, replay_events: Vec<Value>) -> Value {
    json!({
        "response": response,
        "replayEvents": replay_events,
    })
}

pub fn ingress_handle_event_response(
    session_id: Option<String>,
    actions: Vec<DwoOutboundAction>,
) -> Value {
    json!({
        "sessionId": session_id,
        "actions": actions,
    })
}

pub fn automation_run_job_response(record: Value, notifications: Vec<DwoOutboundAction>) -> Value {
    json!({
        "record": record,
        "notifications": notifications,
    })
}

pub fn automation_record_delivery_response() -> Value {
    json!({ "ok": true })
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
            parse_channel_command("/load Main topic"),
            Some(DwoChannelCommand::Load {
                session_ref: "Main topic".to_string()
            })
        );
        assert_eq!(
            parse_channel_command("/set-home s1"),
            Some(DwoChannelCommand::SetHome {
                session_ref: "s1".to_string()
            })
        );
        assert_eq!(
            parse_channel_command("/home"),
            Some(DwoChannelCommand::Home)
        );
        assert_eq!(
            parse_channel_command("/deny c1"),
            Some(DwoChannelCommand::Deny {
                confirmation_id: "c1".to_string()
            })
        );
        assert_eq!(
            parse_channel_command("/switch s1"),
            Some(DwoChannelCommand::Usage(
                "未知命令：/switch\n使用 /help 查看可用命令".to_string()
            ))
        );
        assert_eq!(
            parse_channel_command("/back"),
            Some(DwoChannelCommand::Usage(
                "未知命令：/back\n使用 /help 查看可用命令".to_string()
            ))
        );
    }

    #[test]
    fn channel_command_parser_reports_usage() {
        assert_eq!(
            parse_channel_command("/load"),
            Some(DwoChannelCommand::Usage(
                "用法：/load <name_or_id>".to_string()
            ))
        );
        assert_eq!(
            parse_channel_command("/set-home"),
            Some(DwoChannelCommand::Usage(
                "用法：/set-home <name_or_id>".to_string()
            ))
        );
        assert_eq!(
            parse_channel_command("/deny"),
            Some(DwoChannelCommand::Usage(
                "用法：/deny <confirmation_id>".to_string()
            ))
        );
        assert_eq!(
            parse_channel_command("/deny c1 reason"),
            Some(DwoChannelCommand::Usage(
                "用法：/deny <confirmation_id>".to_string()
            ))
        );
        assert_eq!(
            parse_channel_command("/approve"),
            Some(DwoChannelCommand::Usage(
                "用法：/approve <confirmation_id>".to_string()
            ))
        );
    }

    #[test]
    fn dwo_protocol_uses_camel_case_fields() {
        let run_job: DwoAutomationRunJobRequest =
            serde_json::from_value(json!({ "jobId": "daily" })).unwrap();
        assert_eq!(run_job.job_id, "daily");
        assert!(
            serde_json::from_value::<DwoAutomationRunJobRequest>(json!({ "job_id": "daily" }))
                .is_err()
        );

        let set_config: DwoSessionSetConfigOptionNotification = serde_json::from_value(json!({
            "sessionId": "s1",
            "configId": "model",
            "value": "gpt-5",
        }))
        .unwrap();
        assert_eq!(set_config.session_id, "s1");
        assert_eq!(set_config.config_id, "model");
        assert!(
            serde_json::from_value::<DwoSessionSetConfigOptionNotification>(json!({
                "session_id": "s1",
                "config_id": "model",
                "value": "gpt-5",
            }))
            .is_err()
        );

        let event = DwoIngressEvent {
            channel: DwoIngressChannel::Weixin,
            source: DwoIngressSource {
                id: "u1".to_string(),
                name: None,
            },
            conversation: DwoIngressConversation {
                id: "c1".to_string(),
                kind: None,
                reply_to: Some("m1".to_string()),
                holder: None,
                state_key: Some("default".to_string()),
            },
            text: Some("hello".to_string()),
            attachments: vec![DwoIngressAttachment {
                path: PathBuf::from("image.png"),
                name: None,
                mime_type: Some("image/png".to_string()),
                kind: None,
            }],
            raw: json!({}),
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value.pointer("/conversation/replyTo"), Some(&json!("m1")));
        assert_eq!(
            value.pointer("/conversation/stateKey"),
            Some(&json!("default"))
        );
        assert_eq!(
            value.pointer("/attachments/0/mimeType"),
            Some(&json!("image/png"))
        );
        assert!(value.pointer("/conversation/reply_to").is_none());
        assert!(value.pointer("/attachments/0/mime_type").is_none());

        let action = DwoOutboundAction {
            channel: DwoIngressChannel::Feishu,
            target: "chat".to_string(),
            body: DwoOutboundBody::Media {
                path: PathBuf::from("report.pdf"),
                kind: None,
                file_type: Some("pdf".to_string()),
            },
        };
        let value = serde_json::to_value(action).unwrap();
        assert_eq!(value.pointer("/fileType"), Some(&json!("pdf")));
        assert!(value.pointer("/file_type").is_none());
    }
}
