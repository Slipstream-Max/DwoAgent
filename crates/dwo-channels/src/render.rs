use dwo_agent_service::{
    ActiveToolCall, ClientTranscriptEvent, MessageContent, SessionEventPayload, SessionSnapshot,
};
pub use dwo_command::session_status::{display_path, policy_name};
pub use dwo_command::session_status::{short_session_id, short_session_id_str};
use dwo_context::ContentBlock;
use dwo_tools::QuestionnaireQuestion;
use std::collections::{HashMap, HashSet};

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

/// Render a user prompt as ordered text segments for a channel broadcast.
/// Text blocks pass through and every other block becomes a visible
/// placeholder; images are never forwarded as real media.
pub fn render_user_prompt_segments(content: &MessageContent) -> Vec<String> {
    let mut segments = Vec::new();
    let mut text = String::new();
    for block in content.as_blocks() {
        match block {
            ContentBlock::Text {
                text: block_text, ..
            } => append_prompt_text(&mut text, block_text),
            ContentBlock::Image { .. } => {
                flush_prompt_text(&mut segments, &mut text);
                segments.push("[图片]".to_string());
            }
            ContentBlock::Audio { .. } => append_prompt_text(&mut text, "[音频]"),
            ContentBlock::Resource { .. } => append_prompt_text(&mut text, "[附件]"),
            ContentBlock::ResourceLink { name, .. } => {
                append_prompt_text(&mut text, &format!("[附件: {name}]"))
            }
        }
    }
    flush_prompt_text(&mut segments, &mut text);
    segments
}

/// Render a user prompt as plain text with placeholders for non-text blocks.
pub fn render_user_prompt_text(content: &MessageContent) -> Option<String> {
    let mut text = String::new();
    for segment in render_user_prompt_segments(content) {
        append_prompt_text(&mut text, &segment);
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn append_prompt_text(text: &mut String, piece: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(piece);
}

fn flush_prompt_text(segments: &mut Vec<String>, text: &mut String) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
    text.clear();
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
    last_response: Option<String>,
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
                prompt = render_user_prompt_text(content);
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
                last_response: None,
            });
        }
        let turn = grouped.last_mut().expect("replay turn was just inserted");
        match &event.payload {
            SessionEventPayload::UserPromptSubmitted { content, .. } => {
                if let Some(text) = render_user_prompt_text(content) {
                    turn.prompts.push(text);
                }
            }
            SessionEventPayload::AssistantCompleted { content, .. }
                if !content.trim().is_empty() =>
            {
                turn.last_response = Some(content.trim().to_string());
            }
            _ => {}
        }
    }

    let rendered = grouped
        .into_iter()
        .filter(|turn| excluded_turn_id.is_none_or(|excluded| turn.turn_id != *excluded))
        .filter_map(|turn| {
            if turn.prompts.is_empty() && turn.last_response.is_none() {
                return None;
            }
            let mut sections = Vec::new();
            if !turn.prompts.is_empty() {
                sections.push(format!("User:\n{}", turn.prompts.join("\n\n")));
            }
            if let Some(response) = &turn.last_response {
                sections.push(format!("Assistant:\n{response}"));
            }
            Some(sections.join("\n\n"))
        })
        .collect::<Vec<_>>();
    rendered[rendered.len().saturating_sub(turns)..].to_vec()
}

#[derive(Default)]
pub struct SessionStreamState {
    last_response: Option<String>,
    tools: HashMap<String, ActiveToolCall>,
    permission_messages: HashSet<String>,
    questionnaires_sent: HashSet<String>,
}

impl SessionStreamState {
    /// 每个 AssistantCompleted 都覆盖上一条：只留最后的回答。
    pub fn remember_response(&mut self, content: String) {
        let content = content.trim();
        if !content.is_empty() {
            self.last_response = Some(content.to_string());
        }
    }

    pub fn take_response(&mut self) -> Option<String> {
        self.last_response.take()
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

    /// 问卷是单向通知：同一个 id 只发一次（防重复投递；id 全生命周期
    /// 唯一，不随 turn 清）。
    pub fn mark_questionnaire_sent(&mut self, questionnaire_id: &str) -> bool {
        self.questionnaires_sent
            .insert(questionnaire_id.to_string())
    }

    pub fn forget_tool(&mut self, tool_call_id: &str) {
        self.tools.remove(tool_call_id);
        self.permission_messages.remove(tool_call_id);
    }

    pub fn finish_turn(&mut self) {
        self.last_response = None;
        self.tools.clear();
        self.permission_messages.clear();
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

/// 问卷消息——channel 侧是**单向通知**：发出去就不管了。用户回不回、
/// 回什么都是普通消息；正式作答 / 跳过只存在于 ACP v2 客户端。
pub fn render_questionnaire(questions: &[QuestionnaireQuestion]) -> String {
    let mut lines = vec![format!("📋 问卷 · {} 题", questions.len())];
    for (index, question) in questions.iter().enumerate() {
        lines.push(String::new());
        let suffix = if question.multiple {
            "（可多选）"
        } else {
            ""
        };
        lines.push(format!("{}. {}{suffix}", index + 1, question.question));
        for (option_index, option) in question.options.iter().enumerate() {
            let letter = char::from(b'A' + option_index as u8);
            lines.push(format!("   {letter}. {option}"));
        }
    }
    lines.push(String::new());
    lines.push("想作答可直接回复（如：1A 2B）；不回也没关系。".to_string());
    lines.join("\n")
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
    use dwo_tools::{QuestionnaireQuestion, SessionMode};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    #[test]
    fn responses_keep_only_the_last_message() {
        let mut stream = SessionStreamState::default();
        stream.remember_response("先检查项目".to_string());
        stream.remember_response("  ".to_string());
        stream.remember_response("检查完成".to_string());

        assert_eq!(stream.take_response().as_deref(), Some("检查完成"));
        assert_eq!(stream.take_response(), None);
    }

    #[test]
    fn questionnaire_renders_questions_options_and_hint() {
        let questions = vec![
            QuestionnaireQuestion {
                question: "翻页用着怎么样？".to_string(),
                options: vec!["顺手".to_string(), "要调".to_string()],
                multiple: false,
            },
            QuestionnaireQuestion {
                question: "还要加什么？".to_string(),
                options: vec!["键盘操作".to_string()],
                multiple: true,
            },
        ];

        assert_eq!(
            render_questionnaire(&questions),
            "📋 问卷 · 2 题\n\n1. 翻页用着怎么样？\n   A. 顺手\n   B. 要调\n\n2. 还要加什么？（可多选）\n   A. 键盘操作\n\n想作答可直接回复（如：1A 2B）；不回也没关系。"
        );
    }

    #[test]
    fn questionnaires_are_delivered_once_per_id() {
        let mut stream = SessionStreamState::default();
        assert!(stream.mark_questionnaire_sent("questionnaire-1"));
        assert!(!stream.mark_questionnaire_sent("questionnaire-1"));
        assert!(stream.mark_questionnaire_sent("questionnaire-2"));
    }

    #[test]
    fn status_uses_the_short_session_id() {
        let id = SessionId::parse("session-1234567890abcdef").unwrap();
        assert_eq!(short_session_id(&id), "12345678");
    }

    #[test]
    fn user_prompt_segments_keep_block_order_with_placeholders() {
        let content = MessageContent::blocks(vec![
            ContentBlock::text("look at this"),
            ContentBlock::image("image/png", "SECRETBYTES"),
            ContentBlock::text("thanks"),
        ]);
        let segments = render_user_prompt_segments(&content);
        assert_eq!(segments, ["look at this", "[图片]", "thanks"]);
        assert!(
            segments
                .iter()
                .all(|segment| !segment.contains("SECRETBYTES"))
        );
    }

    #[test]
    fn user_prompt_text_uses_placeholders_instead_of_base64() {
        let content = MessageContent::blocks(vec![
            ContentBlock::text("before"),
            ContentBlock::image("image/png", "!!not-base64!!"),
            ContentBlock::text("after"),
        ]);

        assert_eq!(
            render_user_prompt_text(&content).as_deref(),
            Some("before\n[图片]\nafter")
        );
    }

    #[test]
    fn prompt_image_blocks_stay_placeholders_even_if_undecodable() {
        let content = MessageContent::blocks(vec![ContentBlock::image("image/png", "!!bad!!")]);
        assert_eq!(render_user_prompt_segments(&content), ["[图片]"]);
    }

    #[test]
    fn user_prompt_text_still_renders_plain_text() {
        assert_eq!(
            render_user_prompt_text(&MessageContent::text("  inspect the project  ")).as_deref(),
            Some("inspect the project")
        );
        assert_eq!(render_user_prompt_text(&MessageContent::text("  ")), None);
    }

    #[test]
    fn replay_keeps_only_the_last_response_per_turn() {
        let first = dwo_agent_service::TurnId::new();
        let second = dwo_agent_service::TurnId::new();
        let transcript = vec![
            ClientTranscriptEvent::new(SessionEventPayload::UserPromptSubmitted {
                message_id: MessageId::new(),
                turn_id: first.clone(),
                origin: EndpointId::new(),
                content: MessageContent::text("first question"),
                source: None,
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
                source: None,
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
            "User:\nfirst question\n\nAssistant:\nfirst answer"
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
                dwo_agent_service::SessionWorkspace::External {
                    pwd: PathBuf::from("."),
                },
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
                    source: None,
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
            terminal_views: Vec::new(),
            live_terminal_ids: Vec::new(),
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
