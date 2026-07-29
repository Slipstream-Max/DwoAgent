use serde::{Deserialize, Serialize};

use crate::token::{cap_content_tokens, estimate_content_tokens};
use crate::{
    ContextMessage, MessageKind, MessageRole, SessionContext, SystemPromptBlock,
    estimate_message_tokens,
};

use super::{COMPACT_INSTRUCTION, compact_tool_exchanges};

pub const DEFAULT_RECENT_CONTEXT_TOKENS: u64 = 20_000;
pub const DEFAULT_RECENT_USER_TOKENS: u64 = 5_000;
pub const IMAGE_DOWNGRADE_INSTRUCTION: &str = r#"Convert this conversation into a precise text-only summary for a model that cannot receive images. Describe all relevant information visible in images, including text, UI state, errors, diagrams, and spatial relationships. Preserve user requirements, decisions, completed work, tool results, file paths, identifiers, unresolved problems, and next steps. Do not refer to inaccessible image data."#;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionView {
    pub instruction: String,
    pub messages: Vec<ContextMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionPlan {
    pub view: CompactionView,
    pub front_user_messages: Vec<ContextMessage>,
    pub reserved_messages: Vec<ContextMessage>,
    reserve_was_filtered: bool,
}

impl CompactionPlan {
    pub fn has_compactable_history(&self) -> bool {
        !self.view.messages.is_empty()
    }

    pub fn needs_replacement(&self) -> bool {
        self.has_compactable_history() || self.reserve_was_filtered
    }

    pub(crate) fn into_replacement(
        self,
        system_prompt: &SystemPromptBlock,
        summary: impl Into<String>,
    ) -> Vec<ContextMessage> {
        let mut messages = vec![ContextMessage::system(system_prompt.content.clone())];
        messages.extend(self.front_user_messages);
        let summary = summary.into();
        if !summary.is_empty() {
            messages.push(ContextMessage::summary(summary));
        }
        messages.extend(self.reserved_messages);
        messages
    }
}

#[derive(Debug, Clone)]
pub struct CompactionPlanner {
    recent_context_tokens: u64,
    recent_user_tokens: u64,
}

impl Default for CompactionPlanner {
    fn default() -> Self {
        Self {
            recent_context_tokens: DEFAULT_RECENT_CONTEXT_TOKENS,
            recent_user_tokens: DEFAULT_RECENT_USER_TOKENS,
        }
    }
}

impl CompactionPlanner {
    pub fn new(recent_context_tokens: u64, recent_user_tokens: u64) -> Self {
        Self {
            recent_context_tokens,
            recent_user_tokens,
        }
    }

    pub fn with_recent_context_tokens(mut self, tokens: u64) -> Self {
        self.recent_context_tokens = tokens;
        self
    }

    pub fn with_recent_user_tokens(mut self, tokens: u64) -> Self {
        self.recent_user_tokens = tokens;
        self
    }

    pub fn build(&self, context: &SessionContext) -> CompactionPlan {
        let messages = without_initial_system(&context.messages);
        let cut = find_reserve_cut(messages, self.recent_context_tokens);
        let history = messages[..cut.index].to_vec();
        let mut raw_reserve = Vec::new();
        if let Some(user_index) = cut.split_turn_user {
            raw_reserve.push(messages[user_index].clone());
        }
        raw_reserve.extend_from_slice(&messages[cut.index..]);

        let reserved_messages = compact_tool_exchanges(&raw_reserve);
        let remaining_user_tokens = self
            .recent_user_tokens
            .saturating_sub(reserved_user_tokens(&reserved_messages));
        let front_user_messages =
            select_recent_users(&history, remaining_user_tokens, cut.split_turn_user);
        let reserve_was_filtered = reserved_messages != raw_reserve;

        CompactionPlan {
            view: CompactionView {
                instruction: COMPACT_INSTRUCTION.to_string(),
                messages: history,
            },
            front_user_messages,
            reserved_messages,
            reserve_was_filtered,
        }
    }

    pub fn build_recovery(&self, context: &SessionContext) -> CompactionPlan {
        let plan = self.build(context);
        if plan.has_compactable_history() {
            return plan;
        }

        let messages = without_initial_system(&context.messages);
        let conversation_tokens = messages
            .iter()
            .map(estimate_message_tokens)
            .fold(0, u64::saturating_add);
        if conversation_tokens == 0 {
            return plan;
        }
        let latest_turn_tokens = messages
            .iter()
            .rposition(ContextMessage::is_real_user)
            .map(|index| {
                messages[index..]
                    .iter()
                    .map(estimate_message_tokens)
                    .fold(0, u64::saturating_add)
            })
            .unwrap_or_default();
        Self {
            recent_context_tokens: (conversation_tokens / 2).max(latest_turn_tokens),
            recent_user_tokens: self.recent_user_tokens,
        }
        .build(context)
    }

    pub fn build_image_downgrade(&self, context: &SessionContext) -> CompactionPlan {
        CompactionPlan {
            view: CompactionView {
                instruction: IMAGE_DOWNGRADE_INSTRUCTION.to_string(),
                messages: without_initial_system(&context.messages).to_vec(),
            },
            front_user_messages: Vec::new(),
            reserved_messages: Vec::new(),
            reserve_was_filtered: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReserveCut {
    index: usize,
    split_turn_user: Option<usize>,
}

fn without_initial_system(messages: &[ContextMessage]) -> &[ContextMessage] {
    if messages
        .first()
        .is_some_and(|message| message.role == MessageRole::System)
    {
        &messages[1..]
    } else {
        messages
    }
}

fn find_reserve_cut(messages: &[ContextMessage], budget: u64) -> ReserveCut {
    let total = messages
        .iter()
        .map(estimate_message_tokens)
        .fold(0, u64::saturating_add);
    if total <= budget {
        return ReserveCut {
            index: 0,
            split_turn_user: None,
        };
    }

    let mut suffix_tokens = vec![0_u64; messages.len() + 1];
    for index in (0..messages.len()).rev() {
        suffix_tokens[index] =
            suffix_tokens[index + 1].saturating_add(estimate_message_tokens(&messages[index]));
    }

    let mut current_turn_user = None;
    for (index, message) in messages.iter().enumerate() {
        if message.is_real_user() {
            current_turn_user = Some(index);
            if suffix_tokens[index] <= budget {
                return ReserveCut {
                    index,
                    split_turn_user: None,
                };
            }
            continue;
        }
        if !is_agent_cut_point(message) {
            continue;
        }
        let Some(user_index) = current_turn_user else {
            continue;
        };
        let retained =
            suffix_tokens[index].saturating_add(estimate_message_tokens(&messages[user_index]));
        if retained <= budget {
            return ReserveCut {
                index,
                split_turn_user: Some(user_index),
            };
        }
    }

    let Some(user_index) = messages.iter().rposition(ContextMessage::is_real_user) else {
        return ReserveCut {
            index: messages.len(),
            split_turn_user: None,
        };
    };
    if user_index + 1 == messages.len() {
        ReserveCut {
            index: user_index,
            split_turn_user: None,
        }
    } else {
        ReserveCut {
            index: messages.len(),
            split_turn_user: Some(user_index),
        }
    }
}

fn is_agent_cut_point(message: &ContextMessage) -> bool {
    message.role == MessageRole::Assistant && message.kind == MessageKind::Conversation
}

fn reserved_user_tokens(messages: &[ContextMessage]) -> u64 {
    messages
        .iter()
        .filter(|message| message.is_real_user())
        .map(|message| estimate_content_tokens(&message.content))
        .fold(0, u64::saturating_add)
}

fn select_recent_users(
    history: &[ContextMessage],
    budget: u64,
    duplicated_user_index: Option<usize>,
) -> Vec<ContextMessage> {
    let mut remaining = budget;
    let mut selected = Vec::new();
    for (index, message) in history.iter().enumerate().rev() {
        if remaining == 0 {
            break;
        }
        if !message.is_real_user() || duplicated_user_index == Some(index) {
            continue;
        }
        let content_tokens = estimate_content_tokens(&message.content);
        let mut message = message.clone();
        if content_tokens > remaining {
            message.content = cap_content_tokens(&message.content, remaining);
            if !message.content.is_empty() {
                selected.push(message);
            }
            break;
        }
        remaining = remaining.saturating_sub(content_tokens);
        selected.push(message);
    }
    selected.reverse();
    selected
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{ToolResultRecord, TurnId};

    #[test]
    fn split_turn_keeps_user_with_agent_suffix_and_summarizes_prefix() {
        let mut context = SessionContext::default();
        context.messages.push(ContextMessage::system("system"));
        context.messages.push(ContextMessage::user("question"));
        for part in ["a", "b", "c", "d", "e"] {
            context
                .messages
                .push(ContextMessage::assistant(part, Vec::new()));
        }

        let plan = CompactionPlanner::new(18, 5_000).build(&context);

        assert_eq!(plan.view.messages[0].content, "question");
        assert!(
            plan.view
                .messages
                .iter()
                .any(|message| message.content == "a")
        );
        assert_eq!(plan.reserved_messages[0].content, "question");
        assert!(
            plan.reserved_messages
                .iter()
                .any(|message| message.content == "e")
        );
        assert!(
            !plan
                .front_user_messages
                .iter()
                .any(|message| message.content == "question")
        );
    }

    #[test]
    fn reserve_cut_never_separates_a_tool_result_from_its_call() {
        let mut context = SessionContext::default();
        context.messages.push(ContextMessage::system("system"));
        context.messages.push(ContextMessage::user("question"));
        context.messages.push(ContextMessage::assistant(
            "working",
            vec![json!({"id":"call-1","name":"terminal","arguments":{"command":"echo hi"}})],
        ));
        context
            .messages
            .push(ContextMessage::tool(&ToolResultRecord {
                tool_call_id: "call-1".to_string(),
                tool_name: "terminal".to_string(),
                output: json!({"status":"completed","output":"large output"}),
                model_context: Vec::new(),
            }));
        context
            .messages
            .push(ContextMessage::assistant("done", Vec::new()));

        let plan = CompactionPlanner::new(40, 5_000).build(&context);
        let tool_index = plan
            .reserved_messages
            .iter()
            .position(|message| message.role == MessageRole::Tool);
        if let Some(tool_index) = tool_index {
            assert!(tool_index > 0);
            assert_eq!(
                plan.reserved_messages[tool_index - 1].role,
                MessageRole::Assistant
            );
        }
    }

    #[test]
    fn front_user_budget_subtracts_users_already_in_reserve() {
        let mut context = SessionContext::default();
        context.messages.push(ContextMessage::system("system"));
        for index in 1..=3 {
            let turn = TurnId::parse(format!("turn-{index}")).unwrap();
            context.messages.push(ContextMessage::user(format!(
                "user-{index}-{}",
                "x".repeat(20)
            )));
            context.messages.push(ContextMessage::assistant(
                format!("answer-{index}"),
                Vec::new(),
            ));
            let _ = turn;
        }

        let plan = CompactionPlanner::new(30, 12).build(&context);
        let retained = plan
            .front_user_messages
            .iter()
            .chain(
                plan.reserved_messages
                    .iter()
                    .filter(|message| message.is_real_user()),
            )
            .map(|message| estimate_content_tokens(&message.content))
            .sum::<u64>();
        assert!(retained <= 12 || plan.front_user_messages.is_empty());
    }
}
