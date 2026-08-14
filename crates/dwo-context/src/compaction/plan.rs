use serde::{Deserialize, Serialize};

use crate::token::{cap_content_tokens, estimate_content_tokens};
use crate::{
    ContextMessage, MessageKind, MessageRole, SessionContext, SystemPromptBlock,
    estimate_message_tokens,
};

use super::{COMPACT_INSTRUCTION, compact_tool_exchanges};

pub const DEFAULT_RECENT_CONTEXT_TOKENS: u64 = 20_000;
pub const DEFAULT_RECENT_USER_TOKENS: u64 = 5_000;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_watcher: Option<ContextMessage>,
    reserve_was_filtered: bool,
}

impl CompactionPlan {
    pub fn has_compactable_history(&self) -> bool {
        !self.view.messages.is_empty()
    }

    pub fn needs_replacement(&self) -> bool {
        self.has_compactable_history() || self.reserve_was_filtered
    }

    pub fn project_for_image_input(mut self, allow_image_input: bool) -> Self {
        if allow_image_input {
            return self;
        }
        let (view_messages, view_was_filtered) = project_messages(self.view.messages, false);
        let (front_user_messages, front_was_filtered) =
            project_messages(self.front_user_messages, false);
        let (reserved_messages, reserve_was_filtered) =
            project_messages(self.reserved_messages, false);
        if let Some(message) = self.plan_watcher.take() {
            let projected = message.project_for_image_input(false);
            self.reserve_was_filtered |= projected.as_ref() != Some(&message);
            self.plan_watcher = projected;
        }
        self.reserve_was_filtered |=
            view_was_filtered || front_was_filtered || reserve_was_filtered;
        self.view.messages = view_messages;
        self.front_user_messages = front_user_messages;
        self.reserved_messages = reserved_messages;
        self
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
        messages.extend(self.plan_watcher);
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
        let source = without_initial_system(&context.messages);
        let plan_watcher = source
            .iter()
            .rev()
            .find(|message| message.kind == MessageKind::PlanWatcher)
            .cloned();
        let messages = source
            .iter()
            .filter(|message| message.kind != MessageKind::PlanWatcher)
            .cloned()
            .collect::<Vec<_>>();
        let messages = messages.as_slice();
        let mut cut = find_reserve_cut(messages, self.recent_context_tokens);
        cut.index = align_cut_to_tool_calls(messages, cut.index);
        if cut.split_turn_user.is_some_and(|index| index >= cut.index) {
            cut.split_turn_user = None;
        }
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
            plan_watcher,
            reserve_was_filtered,
        }
    }

    pub fn build_recovery(&self, context: &SessionContext) -> CompactionPlan {
        let plan = self.build(context);
        if plan.has_compactable_history() {
            return plan;
        }

        let messages = without_initial_system(&context.messages)
            .iter()
            .filter(|message| message.kind != MessageKind::PlanWatcher)
            .cloned()
            .collect::<Vec<_>>();
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
}

fn project_messages(
    messages: Vec<ContextMessage>,
    allow_image_input: bool,
) -> (Vec<ContextMessage>, bool) {
    let mut was_filtered = false;
    let projected = messages
        .into_iter()
        .filter_map(|message| {
            let projected = message.project_for_image_input(allow_image_input);
            was_filtered |= projected.as_ref() != Some(&message);
            projected
        })
        .collect();
    (projected, was_filtered)
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

fn align_cut_to_tool_calls(messages: &[ContextMessage], mut cut: usize) -> usize {
    loop {
        let required_call = messages[cut..]
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .filter_map(|result_id| {
                messages[..cut]
                    .iter()
                    .rposition(|candidate| candidate.calls_tool(result_id))
            })
            .min();
        let Some(required_call) = required_call else {
            return cut;
        };
        cut = required_call;
    }
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
    fn item_first_reserve_cut_moves_before_all_calls_needed_by_reserved_results() {
        let mut context = SessionContext::default();
        context.messages.push(ContextMessage::system("system"));
        context.messages.push(ContextMessage::user("question"));
        context.messages.push(ContextMessage::response_item(
            json!({"type":"function_call", "call_id":"call-1", "name":"terminal", "arguments":"{}"}),
            None,
        ));
        context.messages.push(ContextMessage::response_item(
            json!({"type":"function_call", "call_id":"call-2", "name":"terminal", "arguments":"{}"}),
            None,
        ));
        for id in ["call-1", "call-2"] {
            context
                .messages
                .push(ContextMessage::tool(&ToolResultRecord {
                    tool_call_id: id.to_string(),
                    tool_name: "terminal".to_string(),
                    output: json!({"status":"completed","output":"ok"}),
                    model_context: Vec::new(),
                }));
        }

        let suffix_budget = std::iter::once(&context.messages[1])
            .chain(context.messages[3..].iter())
            .map(estimate_message_tokens)
            .sum();
        let plan = CompactionPlanner::new(suffix_budget, 5_000).build(&context);
        let reserved_ids = plan
            .reserved_messages
            .iter()
            .filter_map(|message| {
                message
                    .response_item_value()
                    .and_then(|item| item.get("call_id"))
                    .and_then(serde_json::Value::as_str)
                    .or(message.tool_call_id.as_deref())
            })
            .collect::<Vec<_>>();
        assert_eq!(reserved_ids, ["call-1", "call-2", "call-1", "call-2"]);
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
