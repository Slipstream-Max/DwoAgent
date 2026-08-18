use dwo_agent_service::{
    ActiveToolCall, ClientTranscriptEvent, MessageContent, SessionEventPayload, SessionSnapshot,
};
pub use dwo_command::session_status::{display_path, policy_name};
pub use dwo_command::session_status::{short_session_id, short_session_id_str};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Debug)]
pub enum OutputSegment {
    Thinking(String),
    ToolCall(ActiveToolCall),
    Answer(String),
}

pub fn render_output_segment(segment: &OutputSegment) -> String {
    match segment {
        OutputSegment::Thinking(text) => render_reasoning(text),
        OutputSegment::ToolCall(call) => render_tool_call(call, "tool call id", &call.tool_call_id),
        OutputSegment::Answer(text) => text.clone(),
    }
}

pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut remaining = text;
    let mut chunks = Vec::new();
    while remaining.chars().count() > max_chars {
        let hard_boundary = remaining
            .char_indices()
            .nth(max_chars)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        let preferred_boundary = remaining[..hard_boundary]
            .rfind("\n\n")
            .map(|index| index + 2)
            .filter(|index| remaining[..*index].chars().count() >= max_chars / 2);
        let boundary = preferred_boundary.unwrap_or(hard_boundary);
        chunks.push(remaining[..boundary].to_string());
        remaining = &remaining[boundary..];
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }
    chunks
}

pub fn render_status(snapshot: &SessionSnapshot) -> String {
    dwo_command::session_status::render_status(
        snapshot,
        dwo_command::session_status::SessionIdDisplay::Short,
    )
}

pub fn render_live_user_prompt(content: &MessageContent) -> Option<String> {
    let content = content.to_string();
    let content = content.trim();
    (!content.is_empty()).then(|| format!("User: {content}"))
}

pub fn render_session_replay(snapshot: &SessionSnapshot, turns: usize) -> Vec<String> {
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
pub struct SessionStreamState {
    reasoning: Vec<String>,
    responses: Vec<String>,
    tools: HashMap<String, ActiveToolCall>,
    permission_messages: HashSet<String>,
    presented_tools: HashSet<String>,
    pending_full_responses: VecDeque<PendingFullResponse>,
}

struct PendingFullResponse {
    content: String,
    tool_call_ids: HashSet<String>,
}

impl SessionStreamState {
    pub fn remember_reasoning(&mut self, reasoning: Option<String>) {
        let Some(reasoning) = reasoning else {
            return;
        };
        let reasoning = reasoning.trim();
        if !reasoning.is_empty() {
            self.reasoning.push(reasoning.to_string());
        }
    }

    pub fn take_reasoning(&mut self, max_message_chars: usize) -> Vec<OutputSegment> {
        let reasoning = std::mem::take(&mut self.reasoning).join("\n\n");
        split_reasoning(&reasoning, max_message_chars)
    }

    pub fn remember_response(&mut self, content: String) {
        let content = content.trim();
        if !content.is_empty() {
            self.responses.push(content.to_string());
        }
    }

    pub fn take_response(&mut self) -> Option<String> {
        let response = std::mem::take(&mut self.responses).join("\n\n");
        (!response.is_empty()).then_some(response)
    }

    pub fn remember_full_response(&mut self, content: String, tool_calls: &[ActiveToolCall]) {
        let content = content.trim();
        if content.is_empty() {
            return;
        }
        self.pending_full_responses.push_back(PendingFullResponse {
            content: content.to_string(),
            tool_call_ids: tool_calls
                .iter()
                .map(|call| call.tool_call_id.clone())
                .collect(),
        });
    }

    pub fn mark_tool_presented(&mut self, tool_call_id: &str) {
        self.presented_tools.insert(tool_call_id.to_string());
    }

    pub fn tool_was_presented(&self, tool_call_id: &str) -> bool {
        self.presented_tools.contains(tool_call_id)
    }

    pub fn take_ready_full_responses(&mut self) -> Vec<OutputSegment> {
        let mut responses = Vec::new();
        while self.pending_full_responses.front().is_some_and(|response| {
            response
                .tool_call_ids
                .iter()
                .all(|id| self.presented_tools.contains(id))
        }) {
            let response = self
                .pending_full_responses
                .pop_front()
                .expect("front response was present");
            responses.push(OutputSegment::Answer(response.content));
        }
        responses
    }

    pub fn take_all_full_responses(&mut self) -> Vec<OutputSegment> {
        self.pending_full_responses
            .drain(..)
            .map(|response| OutputSegment::Answer(response.content))
            .collect()
    }

    pub fn remember_tool(&mut self, call: ActiveToolCall) {
        self.tools.insert(call.tool_call_id.clone(), call);
    }

    pub fn tool(&self, tool_call_id: &str) -> Option<&ActiveToolCall> {
        self.tools.get(tool_call_id)
    }

    pub fn mark_permission_sent(&mut self, tool_call_id: &str) -> bool {
        self.permission_messages.insert(tool_call_id.to_string())
    }

    pub fn forget_tool(&mut self, tool_call_id: &str) {
        self.tools.remove(tool_call_id);
        self.permission_messages.remove(tool_call_id);
    }

    pub fn finish_turn(&mut self) {
        self.reasoning.clear();
        self.responses.clear();
        self.tools.clear();
        self.permission_messages.clear();
        self.presented_tools.clear();
        self.pending_full_responses.clear();
    }
}

pub fn render_tool_call(call: &ActiveToolCall, id_label: &str, id: &str) -> String {
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

pub fn render_reasoning(reasoning: &str) -> String {
    format!("🧠 Thinking:\n{}", reasoning.trim())
}

fn split_reasoning(reasoning: &str, max_message_chars: usize) -> Vec<OutputSegment> {
    let mut remaining = reasoning.trim();
    let mut chunks = Vec::new();
    let label_chars = render_reasoning("").chars().count();
    let content_chars = max_message_chars.saturating_sub(label_chars).max(1);
    while !remaining.is_empty() {
        let boundary = remaining
            .char_indices()
            .nth(content_chars)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        let (chunk, rest) = remaining.split_at(boundary);
        chunks.push(OutputSegment::Thinking(chunk.to_string()));
        remaining = rest.trim_start();
    }
    chunks
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
        EndpointId, MessageId, SessionId, SessionLlmSettings, SessionRecord, SessionUsageSnapshot,
    };
    use dwo_tools::SessionMode;
    use serde_json::json;
    use std::path::{Path, PathBuf};

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

        let reasoning = stream
            .take_reasoning(1_800)
            .iter()
            .map(render_output_segment)
            .collect::<Vec<_>>();
        assert_eq!(reasoning, ["🧠 Thinking:\nfirst thought\n\nsecond thought"]);
        assert!(stream.take_reasoning(1_800).is_empty());

        stream.remember_reasoning(Some("next turn".to_string()));
        stream.finish_turn();
        assert!(stream.take_reasoning(1_800).is_empty());
    }

    #[test]
    fn long_reasoning_repeats_the_label_for_each_chunk() {
        let chunks = split_reasoning(&"思".repeat(2_000), 1_800);
        assert_eq!(chunks.len(), 2);
        let chunks = chunks.iter().map(render_output_segment).collect::<Vec<_>>();
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.starts_with("🧠 Thinking:\n"))
        );
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 1_800));
    }

    #[test]
    fn status_uses_the_short_session_id() {
        let id = SessionId::parse("session-1234567890abcdef").unwrap();
        assert_eq!(short_session_id(&id), "12345678");
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

        let status = dwo_command::session_status::render_status(
            &snapshot,
            dwo_command::session_status::SessionIdDisplay::Full,
        );
        assert!(status.contains("ID: session-test"));
        assert!(status.contains("Model: scripted-test-model"));
        assert!(status.contains("Reasoning: default"));

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
            status: "in_progress".to_string(),
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
            status: "in_progress".to_string(),
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
