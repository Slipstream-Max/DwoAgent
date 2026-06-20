//! ACP permission request mapping.

use std::sync::Arc;

use agent_client_protocol::schema::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionId, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use anyhow::Result;
use serde_json::{Map, Value};

use super::mapper::{parse_tool_call_status, parse_tool_kind, render_tool_call_content};
use crate::tools::PermissionRequester;

pub fn permission_requester(cx: &ConnectionTo<Client>, session_id: &str) -> PermissionRequester {
    let cx_for_perm = cx.clone();
    let session_id_for_perm = session_id.to_string();
    Arc::new(move |_target: String, payload: Map<String, Value>| {
        let cx = cx_for_perm.clone();
        let sid = session_id_for_perm.clone();
        Box::pin(async move { request_permission_from_client(&cx, &sid, &payload).await })
    })
}

pub async fn request_permission_from_client(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    payload: &Map<String, Value>,
) -> Result<String> {
    let tool_call_id: ToolCallId = payload
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
        .into();

    let mut fields = ToolCallUpdateFields::new();
    if let Some(status) = payload.get("status").and_then(Value::as_str) {
        fields = fields.status(parse_tool_call_status(status));
    }
    if let Some(title) = payload.get("title").and_then(Value::as_str) {
        fields = fields.title(title.to_string());
    }
    if let Some(kind) = payload.get("kind").and_then(Value::as_str) {
        fields = fields.kind(parse_tool_kind(kind));
    }
    if let Some(raw_input) = payload.get("raw_input") {
        fields = fields.raw_input(raw_input.clone());
    }
    if let Some(raw_output) = payload.get("raw_output") {
        fields = fields.raw_output(raw_output.clone());
    }
    if let Some(content) = render_tool_call_content(
        payload.get("title").and_then(Value::as_str).unwrap_or(""),
        payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending"),
        payload.get("raw_input"),
        payload.get("raw_output"),
    ) {
        fields = fields.content(content);
    }
    let tool_call = ToolCallUpdate::new(tool_call_id, fields);

    let options = vec![
        PermissionOption::new("allow_once", "Allow Once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "reject_once",
            "Reject Once",
            PermissionOptionKind::RejectOnce,
        ),
    ];

    let sid = SessionId::new(session_id);
    let request = RequestPermissionRequest::new(sid, tool_call, options);

    let response = cx
        .send_request_to(Client, request)
        .block_task()
        .await
        .map_err(|e| anyhow::anyhow!("permission request failed: {e}"))?;

    match response.outcome {
        RequestPermissionOutcome::Cancelled => Ok("cancelled".to_string()),
        RequestPermissionOutcome::Selected(selected) => {
            let option_id = selected.option_id.to_string();
            match option_id.as_str() {
                "allow_once" | "reject_once" => Ok(option_id),
                _ => Ok("reject_once".to_string()),
            }
        }
        _ => Ok("reject_once".to_string()),
    }
}
