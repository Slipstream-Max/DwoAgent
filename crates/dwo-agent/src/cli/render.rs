use std::collections::HashSet;
use std::io::Write;

use anyhow::{Context, Result, bail};
use dwo_agent_service::{
    ActiveToolCall, ContentBlock, MessageContent, SessionEvent, SessionEventPayload, SessionRecord,
    SessionSnapshot,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

pub fn print_value(value: &Value) -> Result<()> {
    print!("{}", render_value(value));
    Ok(())
}

pub fn render_value(value: &Value) -> String {
    serde_yaml::to_string(value).unwrap_or_else(|_| "value: <unrenderable>\n".to_string())
}

pub fn print_session_list(value: &Value) -> Result<()> {
    print!("{}", render_session_list(value)?);
    Ok(())
}

fn render_session_list(value: &Value) -> Result<String> {
    let records: Vec<SessionRecord> = serde_json::from_value(value.clone())?;
    if records.is_empty() {
        return Ok("No sessions\n".to_string());
    }

    let mut output = String::new();
    for record in records {
        output.push_str(record.info.id.as_str());
        output.push('\n');
        output.push_str("  title: ");
        output.push_str(&yaml_scalar(&record.info.title));
        output.push('\n');
    }
    Ok(output)
}

pub async fn stream_watch<W: Write>(
    output: W,
    snapshot_value: Value,
    mut events: mpsc::UnboundedReceiver<Value>,
) -> Result<()> {
    let mut renderer = WatchRenderer::new(output);
    renderer.render_snapshot(&snapshot_value)?;
    while let Some(frame) = events.recv().await {
        renderer.render_frame(&frame)?;
    }
    renderer.finish()
}

#[derive(Debug, Deserialize)]
struct EventFrame {
    #[serde(default)]
    params: Option<SessionEvent>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Reasoning,
    Answer,
}

struct WatchRenderer<W> {
    output: W,
    section: Option<Section>,
    assistant_delta_turns: HashSet<String>,
    reasoning_delta_turns: HashSet<String>,
    started_tools: HashSet<String>,
    completed_tools: HashSet<String>,
}

impl<W: Write> WatchRenderer<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            section: None,
            assistant_delta_turns: HashSet::new(),
            reasoning_delta_turns: HashSet::new(),
            started_tools: HashSet::new(),
            completed_tools: HashSet::new(),
        }
    }

    fn render_snapshot(&mut self, value: &Value) -> Result<()> {
        let snapshot_value = value
            .get("snapshot")
            .cloned()
            .context("daemon omitted watch snapshot")?;
        let snapshot: SessionSnapshot = serde_json::from_value(snapshot_value)?;
        self.index_delta_turns(&snapshot);

        writeln!(self.output, "session {}", snapshot.record.info.id)?;
        writeln!(
            self.output,
            "  title: {}",
            yaml_scalar(&snapshot.record.info.title)
        )?;
        writeln!(
            self.output,
            "  model: {}",
            yaml_scalar(&snapshot.record.llm.model)
        )?;
        writeln!(self.output, "  phase: {}", phase_name(snapshot.phase))?;
        writeln!(
            self.output,
            "  usage: {}/{} tokens",
            snapshot.usage.used, snapshot.usage.size
        )?;

        for event in &snapshot.transcript {
            self.render_payload(&event.payload)?;
        }
        if !snapshot.partial_message.is_empty()
            && snapshot
                .active_turn_id
                .as_ref()
                .is_none_or(|turn_id| !self.assistant_delta_turns.contains(&turn_id.to_string()))
        {
            self.write_section_text(Section::Answer, &snapshot.partial_message)?;
        }
        for call in &snapshot.active_tool_calls {
            if self.started_tools.insert(call.tool_call_id.clone()) {
                self.render_tool_started(call)?;
            }
        }
        if let Some(permission) = &snapshot.pending_permission {
            self.end_section()?;
            writeln!(self.output, "permission")?;
            writeln!(self.output, "  id: {}", yaml_scalar(&permission.request_id))?;
            writeln!(
                self.output,
                "  tool: {}",
                yaml_scalar(&permission.tool_name)
            )?;
            writeln!(self.output, "  state: pending")?;
        }
        self.output.flush()?;
        Ok(())
    }

    fn render_frame(&mut self, value: &Value) -> Result<()> {
        let frame: EventFrame = serde_json::from_value(value.clone())?;
        if let Some(error) = frame.error {
            bail!(error);
        }
        if let Some(event) = frame.params {
            self.render_payload(&event.payload)?;
            self.output.flush()?;
        }
        Ok(())
    }

    fn index_delta_turns(&mut self, snapshot: &SessionSnapshot) {
        for event in &snapshot.transcript {
            match &event.payload {
                SessionEventPayload::AssistantDelta { turn_id, .. } => {
                    self.assistant_delta_turns.insert(turn_id.to_string());
                }
                SessionEventPayload::AssistantReasoningDelta { turn_id, .. } => {
                    self.reasoning_delta_turns.insert(turn_id.to_string());
                }
                _ => {}
            }
        }
    }

    fn render_payload(&mut self, payload: &SessionEventPayload) -> Result<()> {
        match payload {
            SessionEventPayload::UserPromptSubmitted { content, .. } => {
                self.end_section()?;
                writeln!(self.output, "user")?;
                write_indented_text(&mut self.output, &content_text(content), 2)?;
            }
            SessionEventPayload::AssistantReasoningDelta { turn_id, delta } => {
                self.reasoning_delta_turns.insert(turn_id.to_string());
                self.write_section_text(Section::Reasoning, delta)?;
            }
            SessionEventPayload::AssistantDelta { turn_id, delta } => {
                self.assistant_delta_turns.insert(turn_id.to_string());
                self.write_section_text(Section::Answer, delta)?;
            }
            SessionEventPayload::AssistantCompleted {
                turn_id,
                content,
                reasoning,
                ..
            } => {
                let turn_id = turn_id.to_string();
                if !self.reasoning_delta_turns.contains(&turn_id) {
                    if let Some(reasoning) = reasoning.as_deref().filter(|text| !text.is_empty()) {
                        self.write_section_text(Section::Reasoning, reasoning)?;
                    }
                }
                if !self.assistant_delta_turns.contains(&turn_id) {
                    self.write_section_text(Section::Answer, content)?;
                }
            }
            SessionEventPayload::ToolStarted { call, .. } => {
                if self.started_tools.insert(call.tool_call_id.clone()) {
                    self.render_tool_started(call)?;
                }
            }
            SessionEventPayload::ToolCompleted { result, .. } => {
                if self.completed_tools.insert(result.tool_call_id.clone()) {
                    self.render_tool_completed(
                        &result.tool_call_id,
                        &result.tool_name,
                        &result.output,
                    )?;
                }
            }
            SessionEventPayload::PermissionRequested { permission, .. } => {
                self.end_section()?;
                writeln!(self.output, "permission")?;
                writeln!(self.output, "  id: {}", yaml_scalar(&permission.request_id))?;
                writeln!(
                    self.output,
                    "  tool: {}",
                    yaml_scalar(&permission.tool_name)
                )?;
                writeln!(self.output, "  state: pending")?;
            }
            SessionEventPayload::PermissionResolved {
                request_id,
                allowed,
                reason,
                ..
            } => {
                self.end_section()?;
                writeln!(self.output, "permission")?;
                writeln!(self.output, "  id: {}", yaml_scalar(request_id))?;
                writeln!(
                    self.output,
                    "  state: {}",
                    if *allowed { "allowed" } else { "denied" }
                )?;
                if let Some(reason) = reason {
                    writeln!(self.output, "  reason: {}", yaml_scalar(reason))?;
                }
            }
            SessionEventPayload::TurnCompleted { .. } => {
                self.render_terminal("completed", None)?;
            }
            SessionEventPayload::TurnCancelled { .. } => {
                self.render_terminal("cancelled", None)?;
            }
            SessionEventPayload::TurnFailed { error, .. } => {
                self.render_terminal("failed", Some(error))?;
            }
            SessionEventPayload::UsageChanged { used, size } => {
                self.end_section()?;
                writeln!(self.output, "usage")?;
                writeln!(self.output, "  current: {used}/{size} tokens")?;
            }
            SessionEventPayload::TitleChanged { title, .. } => {
                self.end_section()?;
                writeln!(self.output, "title")?;
                writeln!(self.output, "  value: {}", yaml_scalar(title))?;
            }
            SessionEventPayload::TurnStarted { .. }
            | SessionEventPayload::ConfigChanged { .. }
            | SessionEventPayload::Closing => {}
        }
        Ok(())
    }

    fn render_tool_started(&mut self, call: &ActiveToolCall) -> Result<()> {
        self.end_section()?;
        writeln!(self.output, "tool")?;
        writeln!(self.output, "  name: {}", yaml_scalar(&call.tool_name))?;
        writeln!(self.output, "  id: {}", yaml_scalar(&call.tool_call_id))?;
        write_yaml_field(&mut self.output, "  input", &call.raw_input)
    }

    fn render_tool_completed(&mut self, id: &str, name: &str, output: &Value) -> Result<()> {
        self.end_section()?;
        writeln!(self.output, "tool_result")?;
        writeln!(self.output, "  name: {}", yaml_scalar(name))?;
        writeln!(self.output, "  id: {}", yaml_scalar(id))?;
        write_yaml_field(&mut self.output, "  output", output)
    }

    fn render_terminal(&mut self, state: &str, error: Option<&str>) -> Result<()> {
        self.end_section()?;
        writeln!(self.output, "terminal")?;
        writeln!(self.output, "  state: {state}")?;
        if let Some(error) = error {
            write_yaml_field(
                &mut self.output,
                "  error",
                &Value::String(error.to_string()),
            )?;
        }
        Ok(())
    }

    fn write_section_text(&mut self, section: Section, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.section != Some(section) {
            if self.section.is_some() {
                write!(self.output, "\n\n")?;
            } else {
                writeln!(self.output)?;
            }
            match section {
                Section::Reasoning => write!(self.output, "reasoning\n  ")?,
                Section::Answer => write!(self.output, "answer\n  ")?,
            }
            self.section = Some(section);
        }
        write!(self.output, "{}", text.replace('\n', "\n  "))?;
        self.output.flush()?;
        Ok(())
    }

    fn end_section(&mut self) -> Result<()> {
        if self.section.take().is_some() {
            writeln!(self.output)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.end_section()?;
        self.output.flush()?;
        Ok(())
    }
}

fn write_yaml_field<W: Write>(output: &mut W, label: &str, value: &Value) -> Result<()> {
    let yaml = render_value(value);
    let lines = yaml.lines().collect::<Vec<_>>();
    if !value.is_object() && !value.is_array() {
        writeln!(
            output,
            "{label}: {}",
            lines.first().copied().unwrap_or("null")
        )?;
    } else {
        writeln!(output, "{label}:")?;
        let child_indent = " ".repeat(
            label
                .chars()
                .take_while(|character| *character == ' ')
                .count()
                + 2,
        );
        for line in lines {
            writeln!(output, "{child_indent}{line}")?;
        }
    }
    Ok(())
}

fn write_indented_text<W: Write>(output: &mut W, text: &str, spaces: usize) -> Result<()> {
    let prefix = " ".repeat(spaces);
    if text.is_empty() {
        writeln!(output, "{prefix}")?;
        return Ok(());
    }
    for line in text.lines() {
        writeln!(output, "{prefix}{line}")?;
    }
    Ok(())
}

fn yaml_scalar(value: &str) -> String {
    let value = value.replace(['\r', '\n'], " ");
    serde_yaml::to_string(&value)
        .unwrap_or_else(|_| format!("\"{}\"", value.replace('"', "\\\"")))
        .trim()
        .to_string()
}

fn phase_name(phase: dwo_agent_service::RuntimePhase) -> &'static str {
    match phase {
        dwo_agent_service::RuntimePhase::Idle => "idle",
        dwo_agent_service::RuntimePhase::Running => "running",
        dwo_agent_service::RuntimePhase::WaitingPermission => "waiting_permission",
        dwo_agent_service::RuntimePhase::Cancelling => "cancelling",
        dwo_agent_service::RuntimePhase::Closing => "closing",
    }
}

fn content_text(content: &MessageContent) -> String {
    content
        .as_blocks()
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.clone(),
            ContentBlock::Image { mime_type, .. } => format!("[image: {mime_type}]"),
            ContentBlock::Audio { mime_type, .. } => format!("[audio: {mime_type}]"),
            ContentBlock::Resource { .. } => "[resource]".to_string(),
            ContentBlock::ResourceLink { name, uri, .. } => format!("[{name}: {uri}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dwo_agent_service::{
        ClientTranscriptEvent, SessionLlmSettings, SessionMode, SessionUsageSnapshot, TurnId,
    };

    use super::*;

    #[test]
    fn renders_values_as_yaml() {
        let rendered =
            render_value(&serde_json::json!({"session_id": "session-1", "usage": {"used": 3}}));
        assert!(rendered.contains("session_id: session-1"));
        assert!(rendered.contains("usage:\n  used: 3"));
        assert!(!rendered.contains('{'));
    }

    #[test]
    fn renders_session_list_as_id_and_title_only() {
        let record = SessionRecord::new(
            dwo_agent_service::SessionId::parse("session-test").unwrap(),
            "Test title".to_string(),
            PathBuf::from("."),
            SessionMode::Confirm,
            SessionLlmSettings::default(),
        );
        let rendered = render_session_list(&serde_json::to_value([record]).unwrap()).unwrap();
        assert_eq!(rendered, "session-test\n  title: Test title\n");
    }

    #[test]
    fn snapshot_does_not_repeat_completed_content_after_deltas() {
        let session_id = dwo_agent_service::SessionId::parse("session-test").unwrap();
        let turn_id = TurnId::parse("turn-test").unwrap();
        let record = SessionRecord::new(
            session_id,
            "Test title".to_string(),
            PathBuf::from("."),
            SessionMode::Confirm,
            SessionLlmSettings::default(),
        );
        let snapshot = SessionSnapshot {
            record,
            transcript: vec![
                ClientTranscriptEvent::new(SessionEventPayload::AssistantDelta {
                    turn_id: turn_id.clone(),
                    delta: "hello".to_string(),
                }),
                ClientTranscriptEvent::new(SessionEventPayload::AssistantCompleted {
                    turn_id: turn_id.clone(),
                    content: "hello".to_string(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                }),
            ],
            usage: SessionUsageSnapshot { used: 1, size: 2 },
            phase: dwo_agent_service::RuntimePhase::Running,
            active_turn_id: Some(turn_id),
            partial_message: "hello".to_string(),
            active_tool_calls: Vec::new(),
            pending_permission: None,
            seq: 2,
        };
        let mut output = Vec::new();
        {
            let mut renderer = WatchRenderer::new(&mut output);
            renderer
                .render_snapshot(&serde_json::json!({"snapshot": snapshot}))
                .unwrap();
            renderer.finish().unwrap();
        }
        let rendered = String::from_utf8(output).unwrap();
        assert_eq!(rendered.matches("hello").count(), 1);
        assert!(rendered.contains("session session-test"));
        assert!(rendered.contains("answer"));
    }

    #[test]
    fn renders_live_reasoning_tools_answer_and_terminal_state() {
        let session_id = dwo_agent_service::SessionId::parse("session-test").unwrap();
        let turn_id = TurnId::parse("turn-test").unwrap();
        let frames = [
            SessionEventPayload::AssistantReasoningDelta {
                turn_id: turn_id.clone(),
                delta: "checking".to_string(),
            },
            SessionEventPayload::ToolStarted {
                turn_id: turn_id.clone(),
                call: ActiveToolCall {
                    tool_call_id: "call-test".to_string(),
                    tool_name: "terminal".to_string(),
                    raw_input: serde_json::json!({"command": "cargo test"}),
                },
            },
            SessionEventPayload::ToolCompleted {
                turn_id: turn_id.clone(),
                result: dwo_tools::ToolResult {
                    tool_call_id: "call-test".to_string(),
                    tool_name: "terminal".to_string(),
                    output: serde_json::json!({"exit_code": 0, "output": "ok"}),
                },
            },
            SessionEventPayload::AssistantDelta {
                turn_id: turn_id.clone(),
                delta: "done".to_string(),
            },
            SessionEventPayload::TurnCompleted { turn_id },
        ];

        let mut output = Vec::new();
        {
            let mut renderer = WatchRenderer::new(&mut output);
            for (seq, payload) in frames.into_iter().enumerate() {
                let event = SessionEvent {
                    seq: seq as u64,
                    session_id: session_id.clone(),
                    payload,
                };
                renderer
                    .render_frame(&serde_json::json!({"method": "session.event", "params": event}))
                    .unwrap();
            }
            renderer.finish().unwrap();
        }

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("reasoning\n  checking"));
        assert!(rendered.contains("tool\n  name: terminal"));
        assert!(rendered.contains("  input:\n    command: cargo test"));
        assert!(rendered.contains("tool_result"));
        assert!(rendered.contains("answer\n  done"));
        assert!(rendered.contains("terminal\n  state: completed"));
        assert!(!rendered.contains("\"method\""));
    }
}
