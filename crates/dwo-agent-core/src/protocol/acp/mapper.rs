//! ACP schema mappers.

use std::collections::HashMap;

use agent_client_protocol::schema::{
    ContentBlock, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionId, SessionInfo, SessionMode, SessionModeState, StopReason, TextContent,
    ToolCallContent, ToolCallStatus, ToolKind,
};
use anyhow::Result;
use serde_json::Value;

use crate::agent::constants::{
    MODE_CONFIRM, MODE_FULL_ACCESS, MODE_WATCH, STOP_CANCELLED, STOP_COMPLETED, STOP_MAX_TURNS,
};
use crate::config::models::{ModelProfile, SessionMetaPayload};
use crate::context::content_block;

pub fn session_info(item: SessionMetaPayload) -> SessionInfo {
    let mut info = SessionInfo::new(
        SessionId::new(item.session_id.as_str()),
        std::path::PathBuf::from(&item.cwd),
    );
    if let Some(title) = item.title {
        info = info.title(title);
    }
    if let Some(updated_at) = item.updated_at {
        info = info.updated_at(updated_at);
    }
    info
}

pub fn build_mode_state(current_mode_id: &str) -> SessionModeState {
    SessionModeState::new(
        current_mode_id.to_string(),
        vec![
            SessionMode::new(MODE_FULL_ACCESS, "Full Access").description(
                "Allow tool calls, except terminal commands denied by policy.".to_string(),
            ),
            SessionMode::new(MODE_CONFIRM, "Confirm").description(
                "Ask for permission unless terminal policy allows or denies the command."
                    .to_string(),
            ),
            SessionMode::new(MODE_WATCH, "Watch").description(
                "Allow only watch-mode terminal commands and read-only tool inspection."
                    .to_string(),
            ),
        ],
    )
}

pub fn build_config_options(
    current_model_id: &str,
    current_mode_id: &str,
    current_reasoning_mode: &str,
    model_profiles: &HashMap<String, ModelProfile>,
) -> Vec<SessionConfigOption> {
    let current_profile = model_profiles.get(current_model_id);

    let policy_option = SessionConfigOption::select(
        "policy_mode",
        "Policy",
        current_mode_id.to_string(),
        vec![
            SessionConfigSelectOption::new(MODE_FULL_ACCESS, "Full Access").description(
                "Allow tool calls, except terminal commands denied by policy.".to_string(),
            ),
            SessionConfigSelectOption::new(MODE_CONFIRM, "Confirm").description(
                "Ask for permission unless terminal policy allows or denies the command."
                    .to_string(),
            ),
            SessionConfigSelectOption::new(MODE_WATCH, "Watch").description(
                "Allow only watch-mode terminal commands and read-only tool inspection."
                    .to_string(),
            ),
        ],
    )
    .description("Choose how tool calls are handled.".to_string())
    .category(SessionConfigOptionCategory::Mode);

    let model_options: Vec<SessionConfigSelectOption> = model_profiles
        .values()
        .map(|profile| {
            SessionConfigSelectOption::new(profile.model_name.clone(), profile.model_name.clone())
                .description(model_description(profile))
        })
        .collect();
    let model_option = SessionConfigOption::select(
        "model",
        "Model",
        current_model_id.to_string(),
        model_options,
    )
    .description("Choose which model this session uses.".to_string())
    .category(SessionConfigOptionCategory::Model);

    let reasoning_options: Vec<SessionConfigSelectOption> = current_profile
        .map(|p| {
            p.reasoning_modes
                .iter()
                .map(|mode| {
                    let mode_str = mode.as_str().to_string();
                    SessionConfigSelectOption::new(mode_str.clone(), mode_str.clone())
                        .description(format!("{} reasoning: {}", current_model_id, mode_str))
                })
                .collect()
        })
        .unwrap_or_default();
    let reasoning_option = SessionConfigOption::select(
        "reasoning_mode",
        "Reasoning Mode",
        current_reasoning_mode.to_string(),
        reasoning_options,
    )
    .description("Choose the reasoning mode for this session.".to_string())
    .category(SessionConfigOptionCategory::ThoughtLevel);

    vec![policy_option, model_option, reasoning_option]
}

pub fn build_config_options_for_snapshot(
    snapshot: &SessionMetaPayload,
    model_profiles: &HashMap<String, ModelProfile>,
) -> Vec<SessionConfigOption> {
    let current_model_id = snapshot
        .pending_model_id
        .as_deref()
        .unwrap_or(&snapshot.model_id);
    let current_reasoning_mode = snapshot
        .pending_reasoning_mode
        .unwrap_or(snapshot.reasoning_mode);
    build_config_options(
        current_model_id,
        snapshot.mode_id.as_str(),
        current_reasoning_mode.as_str(),
        model_profiles,
    )
}

fn model_description(profile: &ModelProfile) -> String {
    let vision = if profile.capabilities.vision {
        "yes"
    } else {
        "no"
    };
    let tools = if profile.capabilities.tool_use {
        "yes"
    } else {
        "no"
    };
    format!(
        "{}/{} | vision: {} | tools: {}",
        profile.config.provider, profile.config.model_id, vision, tools
    )
}

pub fn normalize_prompt_blocks(prompt: &[ContentBlock]) -> Result<(Value, Vec<Value>)> {
    if prompt.is_empty() {
        anyhow::bail!("prompt cannot be empty");
    }

    let mut blocks: Vec<Value> = Vec::new();
    for (index, item) in prompt.iter().enumerate() {
        match item {
            ContentBlock::Text(text) => {
                let text_value = text.text.trim();
                if text_value.is_empty() {
                    continue;
                }
                blocks.push(content_block::text(text_value)?);
            }
            ContentBlock::Image(img) => {
                if img.data.trim().is_empty()
                    && img.uri.as_deref().map(str::trim).unwrap_or("").is_empty()
                {
                    anyhow::bail!("image block at index {index} must provide either data or uri");
                }
                if !img.data.trim().is_empty() {
                    if img.mime_type.trim().is_empty() {
                        anyhow::bail!("image block at index {index} must provide mimeType");
                    }
                    blocks.push(content_block::image_url_data(&img.mime_type, &img.data)?);
                    continue;
                }
                if let Some(uri) = img
                    .uri
                    .as_deref()
                    .map(str::trim)
                    .filter(|uri| !uri.is_empty())
                {
                    blocks.push(content_block::image_url(uri)?);
                }
            }
            ContentBlock::Resource(_) | ContentBlock::ResourceLink(_) => {
                blocks.push(serde_json::to_value(item)?);
            }
            _ => {
                anyhow::bail!("Unsupported prompt block type at index {index}");
            }
        }
    }

    if blocks.is_empty() {
        anyhow::bail!("prompt cannot be empty");
    }

    if blocks.len() == 1
        && let Some(text) = blocks[0].get("text").and_then(Value::as_str)
    {
        return Ok((Value::String(text.to_string()), blocks));
    }
    Ok((Value::Array(blocks.clone()), blocks))
}

pub fn map_stop_reason(stop_reason: &str) -> StopReason {
    match stop_reason {
        STOP_COMPLETED => StopReason::EndTurn,
        STOP_CANCELLED => StopReason::Cancelled,
        STOP_MAX_TURNS => StopReason::MaxTurnRequests,
        _ => StopReason::EndTurn,
    }
}

pub(crate) fn parse_tool_call_status(status: &str) -> ToolCallStatus {
    match status.trim() {
        "pending" => ToolCallStatus::Pending,
        "in_progress" => ToolCallStatus::InProgress,
        "completed" | "completed_success" => ToolCallStatus::Completed,
        "failed" | "completed_error" => ToolCallStatus::Failed,
        _ => ToolCallStatus::Pending,
    }
}

pub(crate) fn parse_tool_kind(kind: &str) -> ToolKind {
    match kind.trim() {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        "switch_mode" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

pub(crate) fn render_tool_call_content(
    _title: &str,
    _status: &str,
    raw_input: Option<&Value>,
    raw_output: Option<&Value>,
) -> Option<Vec<ToolCallContent>> {
    if let Some(output) = raw_output {
        let result_text = format!("```json\n{}\n```", format_json_like(output));
        return Some(vec![ToolCallContent::from(ContentBlock::Text(
            TextContent::new(truncate_text(&result_text, 8000)),
        ))]);
    }

    let mut text = None;
    if let Some(Value::Object(input)) = raw_input {
        if let Some(command) = input.get("command").and_then(Value::as_str)
            && !command.trim().is_empty()
        {
            text = Some(command.to_string());
        }
    }
    if text.is_none() {
        text = raw_input.map(format_json_like);
    }
    text.filter(|s| !s.trim().is_empty()).map(|value| {
        vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
            truncate_text(&value, 8000),
        )))]
    })
}

pub(crate) fn parse_tool_call_content(value: &Value) -> Option<Vec<ToolCallContent>> {
    let items = value.as_array()?;
    let mut out = Vec::new();
    for item in items {
        let content = item.get("content").unwrap_or(item);
        let content_type = content.get("type").and_then(Value::as_str).unwrap_or("");
        if content_type != "text" {
            continue;
        }
        let text = content.get("text").and_then(Value::as_str).unwrap_or("");
        if text.trim().is_empty() {
            continue;
        }
        out.push(ToolCallContent::from(ContentBlock::Text(TextContent::new(
            text.to_string(),
        ))));
    }
    if out.is_empty() { None } else { Some(out) }
}

fn format_json_like(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("\n[TRUNCATED]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ImageContent;
    use serde_json::json;

    #[test]
    fn normalize_prompt_blocks_rejects_empty_prompt() {
        let error = normalize_prompt_blocks(&[]).unwrap_err();
        assert!(error.to_string().contains("prompt cannot be empty"));
    }

    #[test]
    fn normalize_prompt_blocks_rejects_only_empty_text() {
        let prompt = vec![ContentBlock::Text(TextContent::new("   "))];

        let error = normalize_prompt_blocks(&prompt).unwrap_err();

        assert!(error.to_string().contains("prompt cannot be empty"));
    }

    #[test]
    fn normalize_prompt_blocks_ignores_empty_text_around_resource_link() {
        let prompt = vec![
            ContentBlock::Text(TextContent::new("   ")),
            serde_json::from_value(json!({
                "type": "resource_link",
                "uri": "file:///tmp/example.md",
                "name": "example.md",
                "mimeType": "text/markdown"
            }))
            .unwrap(),
            ContentBlock::Text(TextContent::new("  summarize this  ")),
        ];

        let (user_input, user_blocks) = normalize_prompt_blocks(&prompt).unwrap();

        assert_eq!(user_input, Value::Array(user_blocks.clone()));
        assert_eq!(user_blocks[0]["type"], "resource_link");
        assert_eq!(
            user_blocks[1],
            json!({"type": "text", "text": "summarize this"})
        );
    }

    #[test]
    fn normalize_prompt_blocks_accepts_single_text_as_string_input() {
        let prompt = vec![ContentBlock::Text(TextContent::new("hello"))];

        let (user_input, user_blocks) = normalize_prompt_blocks(&prompt).unwrap();

        assert_eq!(user_input, Value::String("hello".to_string()));
        assert_eq!(user_blocks, vec![json!({"type": "text", "text": "hello"})]);
    }

    #[test]
    fn normalize_prompt_blocks_rejects_image_without_data_or_uri() {
        let prompt = vec![ContentBlock::Image(ImageContent::new("", ""))];

        let error = normalize_prompt_blocks(&prompt).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("image block at index 0 must provide either data or uri")
        );
    }

    #[test]
    fn normalize_prompt_blocks_converts_image_data_to_image_url() {
        let prompt = vec![ContentBlock::Image(ImageContent::new("abc", "image/png"))];

        let (user_input, user_blocks) = normalize_prompt_blocks(&prompt).unwrap();

        assert_eq!(user_input, Value::Array(user_blocks.clone()));
        assert_eq!(
            user_blocks,
            vec![json!({
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,abc"}
            })]
        );
    }

    #[test]
    fn render_tool_call_content_prefers_command_input() {
        let content = render_tool_call_content(
            "terminal_exec",
            "pending",
            Some(&json!({"command": "cargo check", "timeout": 30})),
            None,
        )
        .unwrap();

        let rendered = serde_json::to_value(content).unwrap();
        assert_eq!(rendered[0]["content"]["text"], "cargo check");
    }

    #[test]
    fn render_tool_call_content_wraps_raw_output_as_json() {
        let content = render_tool_call_content(
            "terminal_exec",
            "completed",
            None,
            Some(&json!({"status": "completed_success"})),
        )
        .unwrap();

        let rendered = serde_json::to_value(content).unwrap();
        let text = rendered[0]["content"]["text"].as_str().unwrap();
        assert!(text.starts_with("```json\n"));
        assert!(text.contains("\"status\": \"completed_success\""));
    }

    #[test]
    fn parse_tool_call_content_uses_explicit_markdown_content() {
        let content = parse_tool_call_content(&json!([{
            "type": "content",
            "content": {
                "type": "text",
                "text": "Agent Flow:\n\n[tool] terminal_exec"
            }
        }]))
        .unwrap();

        let rendered = serde_json::to_value(content).unwrap();
        assert_eq!(
            rendered[0]["content"]["text"],
            "Agent Flow:\n\n[tool] terminal_exec"
        );
    }
}
