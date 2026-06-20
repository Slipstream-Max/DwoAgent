//! ACP notification mapping.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::schema::{
    ConfigOptionUpdate, ContentBlock, ContentChunk, CurrentModeUpdate, SessionId,
    SessionInfoUpdate, SessionNotification, SessionUpdate, TextContent, ToolCall, ToolCallId,
    ToolCallUpdate, ToolCallUpdateFields, UsageUpdate,
};
use agent_client_protocol::{Client, ConnectionTo};
use serde_json::{Map, Value};

use super::mapper::{
    build_config_options, parse_tool_call_content, parse_tool_call_status, parse_tool_kind,
    render_tool_call_content,
};
use crate::agent::activity::event::{
    EVENT_ACTIVITY_BOX, EVENT_ACTIVITY_BOX_UPDATE, EVENT_AGENT_MESSAGE_CHUNK,
    EVENT_AGENT_THOUGHT_CHUNK, EVENT_CONFIG_OPTION, EVENT_CURRENT_MODE, EVENT_SESSION_INFO,
    EVENT_TOOL_CALL, EVENT_TOOL_CALL_UPDATE, EVENT_USAGE_UPDATE, EVENT_USER_MESSAGE_CHUNK,
};
use crate::config::models::{ContextUsageSnapshot, ModelProfile};
use crate::tools::UpdateEmitter;

pub fn update_emitter(cx: &ConnectionTo<Client>, session_id: &str) -> UpdateEmitter {
    let cx_for_emit = cx.clone();
    let session_id_for_emit = session_id.to_string();
    Arc::new(move |_target: String, update: Map<String, Value>| {
        let cx = cx_for_emit.clone();
        let sid = session_id_for_emit.clone();
        Box::pin(async move {
            emit_session_update(&cx, &sid, &update);
            Ok(())
        })
    })
}

pub fn emit_current_mode_state(cx: &ConnectionTo<Client>, session_id: &str, current_mode_id: &str) {
    let notif = SessionNotification::new(
        SessionId::new(session_id),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(current_mode_id.to_string())),
    );
    let _ = cx.send_notification_to(Client, notif);
}

pub fn emit_mode_and_config_state(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    current_mode_id: &str,
    current_model_id: &str,
    current_reasoning_mode: &str,
    model_profiles: &HashMap<String, ModelProfile>,
) {
    let sid = SessionId::new(session_id);
    let mode_notif = SessionNotification::new(
        sid.clone(),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(current_mode_id.to_string())),
    );
    let _ = cx.send_notification_to(Client, mode_notif);

    let config_notif = SessionNotification::new(
        sid,
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(build_config_options(
            current_model_id,
            current_mode_id,
            current_reasoning_mode,
            model_profiles,
        ))),
    );
    let _ = cx.send_notification_to(Client, config_notif);
}

pub fn emit_context_usage_state(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    usage: ContextUsageSnapshot,
) {
    let notif = SessionNotification::new(
        SessionId::new(session_id),
        SessionUpdate::UsageUpdate(UsageUpdate::new(usage.used, usage.size)),
    );
    let _ = cx.send_notification_to(Client, notif);
}

pub fn spawn_context_usage_update(
    cx: &ConnectionTo<Client>,
    session_id: String,
    session: Arc<crate::agent::session_agent::SessionAgent>,
) {
    let cx_for_usage = cx.clone();
    let _ = cx.spawn(async move {
        let usage = session.context_usage_snapshot().await;
        emit_context_usage_state(&cx_for_usage, &session_id, usage);
        Ok(())
    });
}

pub fn emit_session_info_state(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    title: Option<&str>,
    updated_at: Option<&str>,
) {
    let Some(title) = title else {
        return;
    };
    let info_update = SessionInfoUpdate::new()
        .title(title.to_string())
        .updated_at(updated_at.unwrap_or_default().to_string());
    let notif = SessionNotification::new(
        SessionId::new(session_id),
        SessionUpdate::SessionInfoUpdate(info_update),
    );
    let _ = cx.send_notification_to(Client, notif);
}

pub fn emit_session_update(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    update: &Map<String, Value>,
) {
    let session_update_type = update
        .get("session_update")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    let sid = SessionId::new(session_id);

    let notification = match session_update_type {
        s if s == EVENT_AGENT_MESSAGE_CHUNK => {
            let text = extract_content_text(update);
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            Some(SessionNotification::new(
                sid,
                SessionUpdate::AgentMessageChunk(chunk),
            ))
        }
        s if s == EVENT_AGENT_THOUGHT_CHUNK => {
            let text = extract_content_text(update);
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            Some(SessionNotification::new(
                sid,
                SessionUpdate::AgentThoughtChunk(chunk),
            ))
        }
        s if s == EVENT_USER_MESSAGE_CHUNK => {
            let text = extract_content_text(update);
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            Some(SessionNotification::new(
                sid,
                SessionUpdate::UserMessageChunk(chunk),
            ))
        }
        s if s == EVENT_TOOL_CALL => {
            let tool_call_id: ToolCallId = update
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into();
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            let mut tc = ToolCall::new(tool_call_id, title.clone())
                .kind(parse_tool_kind(
                    update
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("other"),
                ))
                .status(parse_tool_call_status(status));
            if let Some(raw_input) = update.get("raw_input") {
                tc = tc.raw_input(raw_input.clone());
            }
            if let Some(raw_output) = update.get("raw_output") {
                tc = tc.raw_output(raw_output.clone());
            }
            if let Some(content) = update
                .get("content")
                .and_then(parse_tool_call_content)
                .or_else(|| {
                    render_tool_call_content(
                        &title,
                        status,
                        update.get("raw_input"),
                        update.get("raw_output"),
                    )
                })
            {
                tc = tc.content(content);
            }
            Some(SessionNotification::new(sid, SessionUpdate::ToolCall(tc)))
        }
        s if s == EVENT_TOOL_CALL_UPDATE => {
            let tool_call_id: ToolCallId = update
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into();
            let mut fields = ToolCallUpdateFields::new();
            if let Some(status) = update.get("status").and_then(Value::as_str) {
                fields = fields.status(parse_tool_call_status(status));
            }
            if let Some(title) = update.get("title").and_then(Value::as_str) {
                fields = fields.title(title.to_string());
            }
            if let Some(kind) = update.get("kind").and_then(Value::as_str) {
                fields = fields.kind(parse_tool_kind(kind));
            }
            if let Some(raw_input) = update.get("raw_input") {
                fields = fields.raw_input(raw_input.clone());
            }
            if let Some(raw_output) = update.get("raw_output") {
                fields = fields.raw_output(raw_output.clone());
            }
            if let Some(content) = update
                .get("content")
                .and_then(parse_tool_call_content)
                .or_else(|| {
                    render_tool_call_content(
                        update.get("title").and_then(Value::as_str).unwrap_or(""),
                        update
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("pending"),
                        update.get("raw_input"),
                        update.get("raw_output"),
                    )
                })
            {
                fields = fields.content(content);
            }
            let tcu = ToolCallUpdate::new(tool_call_id, fields);
            Some(SessionNotification::new(
                sid,
                SessionUpdate::ToolCallUpdate(tcu),
            ))
        }
        s if s == EVENT_ACTIVITY_BOX => {
            let tool_call_id: ToolCallId = update
                .get("activity_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into();
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Activity")
                .to_string();
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("in_progress");
            let mut tc = ToolCall::new(tool_call_id, title)
                .kind(parse_tool_kind(
                    update
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("think"),
                ))
                .status(parse_tool_call_status(status));
            if let Some(content) = update.get("content").and_then(parse_tool_call_content) {
                tc = tc.content(content);
            }
            Some(SessionNotification::new(sid, SessionUpdate::ToolCall(tc)))
        }
        s if s == EVENT_ACTIVITY_BOX_UPDATE => {
            let tool_call_id: ToolCallId = update
                .get("activity_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into();
            let mut fields = ToolCallUpdateFields::new();
            if let Some(status) = update.get("status").and_then(Value::as_str) {
                fields = fields.status(parse_tool_call_status(status));
            }
            if let Some(title) = update.get("title").and_then(Value::as_str) {
                fields = fields.title(title.to_string());
            }
            if let Some(kind) = update.get("kind").and_then(Value::as_str) {
                fields = fields.kind(parse_tool_kind(kind));
            }
            if let Some(content) = update.get("content").and_then(parse_tool_call_content) {
                fields = fields.content(content);
            }
            let tcu = ToolCallUpdate::new(tool_call_id, fields);
            Some(SessionNotification::new(
                sid,
                SessionUpdate::ToolCallUpdate(tcu),
            ))
        }
        s if s == EVENT_CURRENT_MODE => {
            let mode_id = update
                .get("current_mode_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(SessionNotification::new(
                sid,
                SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(mode_id)),
            ))
        }
        s if s == EVENT_CONFIG_OPTION => Some(SessionNotification::new(
            sid,
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(Vec::new())),
        )),
        s if s == EVENT_USAGE_UPDATE => {
            let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
            let size = update.get("size").and_then(Value::as_u64).unwrap_or(0);
            Some(SessionNotification::new(
                sid,
                SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)),
            ))
        }
        s if s == EVENT_SESSION_INFO => {
            let mut info_update = SessionInfoUpdate::new();
            if let Some(title) = update.get("title").and_then(Value::as_str) {
                info_update = info_update.title(title.to_string());
            }
            if let Some(updated_at) = update.get("updated_at").and_then(Value::as_str) {
                info_update = info_update.updated_at(updated_at.to_string());
            }
            Some(SessionNotification::new(
                sid,
                SessionUpdate::SessionInfoUpdate(info_update),
            ))
        }
        _ => None,
    };

    if let Some(notif) = notification {
        let _ = cx.send_notification_to(Client, notif);
    }
}

fn extract_content_text(update: &Map<String, Value>) -> String {
    update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
