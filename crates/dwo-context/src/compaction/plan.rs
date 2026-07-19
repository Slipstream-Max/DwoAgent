use serde::{Deserialize, Serialize};

use crate::{ContextMessage, MessageKind, MessageRole, SessionContext, SystemPromptBlock};

use super::{COMPACT_INSTRUCTION, OMITTED_MARKER, compact_tool_exchanges};

pub const DEFAULT_RECENT_USER_BYTES: usize = 20_000;
pub const DEFAULT_RECENT_TURNS: usize = 3;
pub const IMAGE_DOWNGRADE_INSTRUCTION: &str = r#"Convert this conversation into a precise text-only summary for a model that cannot receive images. Describe all relevant information visible in images, including text, UI state, errors, diagrams, and spatial relationships. Preserve user requirements, decisions, completed work, tool results, file paths, identifiers, unresolved problems, and next steps. Do not refer to inaccessible image data."#;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionView {
    pub instruction: String,
    pub messages: Vec<ContextMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionPlan {
    pub view: CompactionView,
    pub recent_user_messages: Vec<ContextMessage>,
    pub recent_turn_messages: Vec<ContextMessage>,
    tail_was_filtered: bool,
}

impl CompactionPlan {
    pub fn has_compactable_history(&self) -> bool {
        !self.view.messages.is_empty()
    }

    pub fn needs_replacement(&self) -> bool {
        self.has_compactable_history() || self.tail_was_filtered
    }

    pub(crate) fn into_replacement(
        self,
        system_prompt: &SystemPromptBlock,
        summary: impl Into<String>,
    ) -> Vec<ContextMessage> {
        let mut messages = vec![ContextMessage::system(system_prompt.content.clone())];
        messages.extend(self.recent_user_messages);
        let summary = summary.into();
        if !summary.is_empty() {
            messages.push(ContextMessage::summary(summary));
        }
        messages.extend(self.recent_turn_messages);
        messages
    }
}

#[derive(Debug, Clone)]
pub struct CompactionPlanner {
    recent_user_bytes: usize,
    recent_turns: usize,
}

impl Default for CompactionPlanner {
    fn default() -> Self {
        Self {
            recent_user_bytes: DEFAULT_RECENT_USER_BYTES,
            recent_turns: DEFAULT_RECENT_TURNS,
        }
    }
}

impl CompactionPlanner {
    pub fn new(recent_user_bytes: usize) -> Self {
        Self {
            recent_user_bytes,
            recent_turns: DEFAULT_RECENT_TURNS,
        }
    }

    pub fn with_recent_turns(mut self, recent_turns: usize) -> Self {
        self.recent_turns = recent_turns;
        self
    }

    pub fn build(&self, context: &SessionContext) -> CompactionPlan {
        let raw_split = recent_turn_split(&context.messages, self.recent_turns);

        let tool_filtered = compact_tool_exchanges(&context.messages);
        let filtered_split = recent_turn_split(&tool_filtered, self.recent_turns);
        let (history, recent_turns) = tool_filtered.split_at(filtered_split);
        let history = filter_history(history);
        let mut recent_turn_messages = filter_recent_turns(recent_turns);

        let remaining_user_bytes =
            cap_user_messages(&mut recent_turn_messages, self.recent_user_bytes);
        let recent_user_messages = select_recent_users(&history, remaining_user_bytes);
        let tail_was_filtered = recent_turn_messages != context.messages[raw_split..];

        CompactionPlan {
            view: CompactionView {
                instruction: COMPACT_INSTRUCTION.to_string(),
                messages: history,
            },
            recent_user_messages,
            recent_turn_messages,
            tail_was_filtered,
        }
    }

    pub fn build_image_downgrade(&self, context: &SessionContext) -> CompactionPlan {
        let mut plan = Self {
            recent_user_bytes: self.recent_user_bytes,
            recent_turns: 0,
        }
        .build(context);
        let tool_filtered = compact_tool_exchanges(&context.messages);
        plan.view = CompactionView {
            instruction: IMAGE_DOWNGRADE_INSTRUCTION.to_string(),
            messages: filter_summary_history(&tool_filtered, false),
        };
        plan
    }
}

fn filter_history(messages: &[ContextMessage]) -> Vec<ContextMessage> {
    filter_summary_history(messages, true)
}

fn filter_summary_history(messages: &[ContextMessage], remove_images: bool) -> Vec<ContextMessage> {
    messages
        .iter()
        .filter(|message| !should_drop_from_history(message))
        .filter_map(|message| {
            let mut message = message.clone();
            message.reasoning = None;
            if remove_images {
                message.content.remove_images();
            }
            (!message.content.is_empty() || !message.tool_calls.is_empty()).then_some(message)
        })
        .collect()
}

fn filter_recent_turns(messages: &[ContextMessage]) -> Vec<ContextMessage> {
    messages
        .iter()
        .filter(|message| !should_drop_from_recent_turns(message))
        .cloned()
        .collect()
}

fn should_drop_from_history(message: &ContextMessage) -> bool {
    should_drop_from_recent_turns(message)
}

fn should_drop_from_recent_turns(message: &ContextMessage) -> bool {
    message.role == MessageRole::System
        || matches!(
            message.kind,
            MessageKind::EnvWatcher
                | MessageKind::Permission
                | MessageKind::Config
                | MessageKind::Runtime
        )
}

fn recent_turn_split(messages: &[ContextMessage], recent_turns: usize) -> usize {
    if recent_turns == 0 {
        return messages.len();
    }
    let mut seen = 0;
    for (index, message) in messages.iter().enumerate().rev() {
        if message.is_real_user() {
            seen += 1;
            if seen == recent_turns {
                return index;
            }
        }
    }
    messages
        .iter()
        .position(ContextMessage::is_real_user)
        .unwrap_or(messages.len())
}

fn cap_user_messages(messages: &mut [ContextMessage], budget: usize) -> usize {
    let mut remaining = budget;
    for message in messages
        .iter_mut()
        .rev()
        .filter(|message| message.is_real_user())
    {
        let bytes = message.content.text_bytes();
        if bytes > remaining {
            message.content.cap_text_bytes(remaining, cap_utf8);
            remaining = 0;
        } else {
            remaining = remaining.saturating_sub(bytes);
        }
    }
    remaining
}

fn select_recent_users(messages: &[ContextMessage], budget: usize) -> Vec<ContextMessage> {
    let mut remaining = budget;
    let mut selected = Vec::new();
    for message in messages
        .iter()
        .rev()
        .filter(|message| message.is_real_user() && !message.content.is_empty())
    {
        if remaining == 0 {
            break;
        }
        let mut message = message.clone();
        if message.content.text_bytes() > remaining {
            message.content.cap_text_bytes(remaining, cap_utf8);
            selected.push(message);
            break;
        }
        remaining = remaining.saturating_sub(message.content.text_bytes());
        selected.push(message);
    }
    selected.reverse();
    selected
}

fn cap_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    if limit == 0 {
        return String::new();
    }
    if limit <= OMITTED_MARKER.len() {
        return value[..floor_char_boundary(value, limit)].to_string();
    }
    let content_budget = limit - OMITTED_MARKER.len();
    let head_budget = content_budget / 2;
    let tail_budget = content_budget.saturating_sub(head_budget);
    let head_end = floor_char_boundary(value, head_budget);
    let tail_start = ceil_char_boundary(value, value.len().saturating_sub(tail_budget));
    format!(
        "{}{}{}",
        &value[..head_end],
        OMITTED_MARKER,
        &value[tail_start..]
    )
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}
