use std::collections::{HashMap, HashSet};
use std::path::Path;

use dwo_agent_service::{
    ActiveToolCall, ClientTranscriptEvent, MessageContent, SessionEventPayload, SessionSnapshot,
};
use dwo_tools::SessionMode;

pub(crate) fn policy_name(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::FullAccess => "full_access",
        SessionMode::Confirm => "confirm",
        SessionMode::Watch => "watch",
    }
}

pub(crate) fn display_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if let Some(path) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = raw.strip_prefix(r"\\?\") {
        path.to_string()
    } else {
        raw.into_owned()
    }
}

pub(crate) fn render_status(snapshot: &SessionSnapshot) -> String {
    let mut lines = vec![format!(
        "Session: {}\nCwd: {}\nPolicy: {}\nModel: {}\nReasoning: {}\nState: {:?}",
        snapshot.record.info.title,
        display_path(&snapshot.record.info.cwd),
        policy_name(snapshot.record.info.mode),
        snapshot.record.llm.model,
        snapshot
            .record
            .llm
            .reasoning
            .as_deref()
            .unwrap_or("default"),
        snapshot.phase,
    )];
    if !snapshot.partial_message.is_empty() {
        lines.push(format!("Current: {}", snapshot.partial_message));
    }
    if let Some(permission) = &snapshot.pending_permission {
        lines.push(format!("Pending permission: {}", permission.request_id));
    }
    lines.join("\n")
}

pub(crate) fn render_live_user_prompt(content: &MessageContent) -> Option<String> {
    let content = content.to_string();
    let content = content.trim();
    (!content.is_empty()).then(|| format!("User: {content}"))
}

pub(crate) fn render_session_replay(snapshot: &SessionSnapshot, turns: usize) -> Vec<String> {
    let running = render_running_turn_replay(snapshot);
    let history_turns = turns.saturating_sub(usize::from(running.is_some()));
    let mut replay = render_replay_turns(
        &snapshot.transcript,
        history_turns,
        snapshot.active_turn_id.as_ref(),
    );
    replay.extend(running);
    replay
}

struct ReplayTurn {
    turn_id: dwo_agent_service::TurnId,
    prompts: Vec<String>,
    responses: Vec<String>,
}

fn render_running_turn_replay(snapshot: &SessionSnapshot) -> Option<String> {
    let active_turn_id = snapshot.active_turn_id.as_ref()?;
    let mut prompt = None;
    let mut latest_reasoning = None;
    let mut current_reasoning = String::new();
    for event in &snapshot.transcript {
        match &event.payload {
            SessionEventPayload::UserPromptSubmitted {
                turn_id, content, ..
            } if turn_id == active_turn_id && prompt.is_none() => {
                prompt = Some(content.to_string());
            }
            SessionEventPayload::AssistantReasoningDelta { turn_id, delta, .. }
                if turn_id == active_turn_id =>
            {
                current_reasoning.push_str(delta);
            }
            SessionEventPayload::AssistantCompleted {
                turn_id, reasoning, ..
            } if turn_id == active_turn_id => {
                let reasoning = if current_reasoning.trim().is_empty() {
                    reasoning.as_deref().unwrap_or("")
                } else {
                    current_reasoning.as_str()
                };
                if !reasoning.trim().is_empty() {
                    latest_reasoning = Some(reasoning.trim().to_string());
                }
                current_reasoning.clear();
            }
            _ => {}
        }
    }
    if !current_reasoning.trim().is_empty() {
        latest_reasoning = Some(current_reasoning.trim().to_string());
    }
    if let Some(step) = snapshot.active_step.as_ref()
        && !step.reasoning.trim().is_empty()
    {
        latest_reasoning = Some(step.reasoning.trim().to_string());
    }

    let mut sections = Vec::new();
    if let Some(prompt) = prompt.filter(|prompt| !prompt.trim().is_empty()) {
        sections.push(format!("User:\n{}", prompt.trim()));
    }
    if let Some(reasoning) = latest_reasoning {
        sections.push(format!("Reasoning:\n{reasoning}"));
    }
    if let Some(response) = snapshot
        .active_step
        .as_ref()
        .map(|step| step.response.trim())
        .filter(|response| !response.is_empty())
    {
        sections.push(format!("Assistant:\n{response}"));
    }
    sections.push("Prompt turn is running".to_string());
    Some(sections.join("\n\n"))
}

fn render_replay_turns(
    transcript: &[ClientTranscriptEvent],
    turns: usize,
    excluded_turn_id: Option<&dwo_agent_service::TurnId>,
) -> Vec<String> {
    let mut grouped = Vec::<ReplayTurn>::new();
    for event in transcript {
        let turn_id = match &event.payload {
            SessionEventPayload::UserPromptSubmitted { turn_id, .. }
            | SessionEventPayload::AssistantCompleted { turn_id, .. } => turn_id,
            _ => continue,
        };
        if grouped.last().is_none_or(|turn| turn.turn_id != *turn_id) {
            grouped.push(ReplayTurn {
                turn_id: turn_id.clone(),
                prompts: Vec::new(),
                responses: Vec::new(),
            });
        }
        let turn = grouped.last_mut().expect("replay turn was just inserted");
        match &event.payload {
            SessionEventPayload::UserPromptSubmitted { content, .. } => {
                turn.prompts.push(content.to_string())
            }
            SessionEventPayload::AssistantCompleted { content, .. }
                if !content.trim().is_empty() =>
            {
                turn.responses.push(content.trim().to_string());
            }
            _ => {}
        }
    }

    let rendered = grouped
        .into_iter()
        .filter(|turn| excluded_turn_id.is_none_or(|excluded| turn.turn_id != *excluded))
        .filter_map(|turn| {
            if turn.prompts.is_empty() && turn.responses.is_empty() {
                return None;
            }
            let mut sections = Vec::new();
            if !turn.prompts.is_empty() {
                sections.push(format!("User:\n{}", turn.prompts.join("\n\n")));
            }
            if !turn.responses.is_empty() {
                sections.push(format!("Assistant:\n{}", turn.responses.join("\n\n")));
            }
            Some(sections.join("\n\n"))
        })
        .collect::<Vec<_>>();
    rendered[rendered.len().saturating_sub(turns)..].to_vec()
}

#[derive(Default)]
pub(crate) struct SessionStreamState {
    reasoning: Vec<String>,
    responses: Vec<String>,
    tools: HashMap<String, ActiveToolCall>,
    permission_messages: HashSet<String>,
}

impl SessionStreamState {
    pub(crate) fn remember_reasoning(&mut self, reasoning: Option<String>) {
        let Some(reasoning) = reasoning else {
            return;
        };
        let reasoning = reasoning.trim();
        if !reasoning.is_empty() {
            self.reasoning.push(reasoning.to_string());
        }
    }

    pub(crate) fn take_reasoning(&mut self) -> Option<String> {
        let reasoning = std::mem::take(&mut self.reasoning).join("\n\n");
        (!reasoning.is_empty()).then(|| render_reasoning(&reasoning))
    }

    pub(crate) fn remember_response(&mut self, content: String) {
        let content = content.trim();
        if !content.is_empty() {
            self.responses.push(content.to_string());
        }
    }

    pub(crate) fn take_response(&mut self) -> Option<String> {
        let response = std::mem::take(&mut self.responses).join("\n\n");
        (!response.is_empty()).then_some(response)
    }

    pub(crate) fn remember_tool(&mut self, call: ActiveToolCall) {
        self.tools.entry(call.tool_call_id.clone()).or_insert(call);
    }

    pub(crate) fn tool(&self, tool_call_id: &str) -> Option<&ActiveToolCall> {
        self.tools.get(tool_call_id)
    }

    pub(crate) fn mark_permission_sent(&mut self, tool_call_id: &str) -> bool {
        self.permission_messages.insert(tool_call_id.to_string())
    }

    pub(crate) fn forget_tool(&mut self, tool_call_id: &str) {
        self.tools.remove(tool_call_id);
        self.permission_messages.remove(tool_call_id);
    }

    pub(crate) fn finish_turn(&mut self) {
        self.reasoning.clear();
        self.responses.clear();
        self.tools.clear();
        self.permission_messages.clear();
    }
}

pub(crate) fn render_tool_call(call: &ActiveToolCall, id_label: &str, id: &str) -> String {
    let content = match call.tool_name.as_str() {
        "terminal" | "terminal_exec" => call
            .raw_input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        "file_edit" => call
            .raw_input
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
    .unwrap_or_else(|| {
        serde_json::to_string_pretty(&call.raw_input).unwrap_or_else(|_| call.tool_name.clone())
    });
    format!("🔧Tool Call:\n{}\n{id_label}：{id}", fenced(&content))
}

pub(crate) fn render_reasoning(reasoning: &str) -> String {
    format!("🧠 Thinking:\n{}", reasoning.trim())
}

fn fenced(content: &str) -> String {
    let mut longest_run = 0usize;
    let mut current_run = 0usize;
    for character in content.chars() {
        if character == '`' {
            current_run += 1;
            longest_run = longest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    let fence = "`".repeat(longest_run.saturating_add(1).max(3));
    format!("{fence}\n{content}\n{fence}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dwo_agent_service::{
        EndpointId, MessageId, SessionLlmSettings, SessionRecord, SessionUsageSnapshot,
    };
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn responses_are_buffered_in_commit_order() {
        let mut stream = SessionStreamState::default();
        stream.remember_response("先检查项目".to_string());
        stream.remember_response("  ".to_string());
        stream.remember_response("检查完成".to_string());

        assert_eq!(
            stream.take_response().as_deref(),
            Some("先检查项目\n\n检查完成")
        );
        assert_eq!(stream.take_response(), None);
    }

    #[test]
    fn reasoning_is_buffered_and_cleared_with_a_turn() {
        let mut stream = SessionStreamState::default();
        stream.remember_reasoning(Some("first thought".to_string()));
        stream.remember_reasoning(Some("  ".to_string()));
        stream.remember_reasoning(Some("second thought".to_string()));

        assert_eq!(
            stream.take_reasoning().as_deref(),
            Some("🧠 Thinking:\nfirst thought\n\nsecond thought")
        );
        assert_eq!(stream.take_reasoning(), None);

        stream.remember_reasoning(Some("next turn".to_string()));
        stream.finish_turn();
        assert_eq!(stream.take_reasoning(), None);
    }

    #[test]
    fn live_user_prompt_is_labeled_for_channel_observers() {
        assert_eq!(
            render_live_user_prompt(&MessageContent::text("  inspect the project  ")).as_deref(),
            Some("User: inspect the project")
        );
        assert_eq!(
            render_live_user_prompt(&MessageContent::text("first line\nsecond line")).as_deref(),
            Some("User: first line\nsecond line")
        );
        assert_eq!(render_live_user_prompt(&MessageContent::text("  ")), None);
    }

    #[test]
    fn replay_groups_user_and_responses_by_turn() {
        let first = dwo_agent_service::TurnId::new();
        let second = dwo_agent_service::TurnId::new();
        let transcript = vec![
            ClientTranscriptEvent::new(SessionEventPayload::UserPromptSubmitted {
                message_id: MessageId::new(),
                turn_id: first.clone(),
                origin: EndpointId::new(),
                content: MessageContent::text("first question"),
            }),
            ClientTranscriptEvent::new(SessionEventPayload::AssistantCompleted {
                message_id: MessageId::new(),
                thought_message_id: MessageId::new(),
                turn_id: first.clone(),
                content: "intermediate".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
            }),
            ClientTranscriptEvent::new(SessionEventPayload::AssistantCompleted {
                message_id: MessageId::new(),
                thought_message_id: MessageId::new(),
                turn_id: first,
                content: "first answer".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
            }),
            ClientTranscriptEvent::new(SessionEventPayload::UserPromptSubmitted {
                message_id: MessageId::new(),
                turn_id: second.clone(),
                origin: EndpointId::new(),
                content: MessageContent::text("second question"),
            }),
            ClientTranscriptEvent::new(SessionEventPayload::AssistantCompleted {
                message_id: MessageId::new(),
                thought_message_id: MessageId::new(),
                turn_id: second,
                content: "second answer".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
            }),
        ];

        assert_eq!(
            render_replay_turns(&transcript, 1, None),
            vec!["User:\nsecond question\n\nAssistant:\nsecond answer"]
        );
        assert_eq!(
            render_replay_turns(&transcript, 10, None)[0],
            "User:\nfirst question\n\nAssistant:\nintermediate\n\nfirst answer"
        );
    }

    #[test]
    fn replay_replaces_the_active_turn_with_prompt_latest_reasoning_and_running_notice() {
        let session_id = dwo_agent_service::SessionId::parse("session-test").unwrap();
        let turn_id = dwo_agent_service::TurnId::parse("turn-test").unwrap();
        let snapshot = SessionSnapshot {
            record: SessionRecord::new(
                session_id,
                "Test".to_string(),
                PathBuf::from("."),
                SessionMode::Confirm,
                SessionLlmSettings::default(),
                dwo_agent_service::DEFAULT_MAX_MODEL_STEPS,
            ),
            transcript: vec![
                ClientTranscriptEvent::new(SessionEventPayload::UserPromptSubmitted {
                    message_id: MessageId::new(),
                    turn_id: turn_id.clone(),
                    origin: EndpointId::new(),
                    content: MessageContent::text("inspect the project"),
                }),
                ClientTranscriptEvent::new(SessionEventPayload::AssistantReasoningDelta {
                    message_id: MessageId::new(),
                    turn_id: turn_id.clone(),
                    step_id: 1,
                    revision: 1,
                    delta: "old reasoning".to_string(),
                }),
                ClientTranscriptEvent::new(SessionEventPayload::AssistantCompleted {
                    message_id: MessageId::new(),
                    thought_message_id: MessageId::new(),
                    turn_id: turn_id.clone(),
                    content: String::new(),
                    reasoning: Some("duplicated old reasoning".to_string()),
                    tool_calls: Vec::new(),
                }),
                ClientTranscriptEvent::new(SessionEventPayload::AssistantReasoningDelta {
                    message_id: MessageId::new(),
                    turn_id: turn_id.clone(),
                    step_id: 2,
                    revision: 1,
                    delta: "latest ".to_string(),
                }),
                ClientTranscriptEvent::new(SessionEventPayload::AssistantReasoningDelta {
                    message_id: MessageId::new(),
                    turn_id: turn_id.clone(),
                    step_id: 2,
                    revision: 2,
                    delta: "reasoning".to_string(),
                }),
            ],
            checkpoint_cursor: 5,
            usage: SessionUsageSnapshot { used: 1, size: 2 },
            phase: dwo_agent_service::RuntimePhase::Running,
            active_turn_id: Some(turn_id),
            active_step: None,
            partial_message: "ignored partial answer".to_string(),
            active_tool_calls: Vec::new(),
            pending_permission: None,
            seq: 1,
        };

        assert_eq!(
            render_session_replay(&snapshot, 5),
            vec![
                "User:\ninspect the project\n\nReasoning:\nlatest reasoning\n\nPrompt turn is running"
            ]
        );
    }

    #[test]
    fn terminal_and_file_edit_calls_render_their_useful_arguments() {
        let terminal = ActiveToolCall {
            tool_call_id: "call-terminal".to_string(),
            tool_name: "terminal".to_string(),
            raw_input: json!({"action":"run", "command":"ls -a"}),
        };
        let rendered = render_tool_call(&terminal, "request id", "permission-1");
        assert_eq!(
            rendered,
            "🔧Tool Call:\n```\nls -a\n```\nrequest id：permission-1"
        );

        let file_edit = ActiveToolCall {
            tool_call_id: "call-edit".to_string(),
            tool_name: "file_edit".to_string(),
            raw_input: json!({"patch":"*** Begin Patch\n```\n*** End Patch"}),
        };
        let rendered = render_tool_call(&file_edit, "tool call id", "call-edit");
        assert!(rendered.contains("````\n*** Begin Patch\n```\n*** End Patch\n````"));
    }

    #[test]
    fn windows_verbatim_paths_are_hidden_in_user_facing_text() {
        let path = Path::new(r"\\?\C:\Users\Example User\paper.pdf");
        assert_eq!(display_path(path), r"C:\Users\Example User\paper.pdf");
    }
}
