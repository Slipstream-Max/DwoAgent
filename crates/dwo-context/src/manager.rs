use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compaction::{CompactionPlan, CompactionPlanner};
use crate::env_watcher::{DynamicEnvironmentSnapshot, EnvWatcherState};
use crate::prompt::{PromptBuildError, RuleSource, SystemPromptBlock, SystemPromptBuilder};
use crate::{
    ContextMessage, MessageContent, MessageKind, ToolResultRecord, estimate_message_tokens,
    estimate_tool_tokens,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Estimated tokens in the complete model request context.
    #[serde(default)]
    pub current_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionState {
    pub count: u64,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionContext {
    #[serde(default)]
    pub system_prompt: SystemPromptBlock,
    #[serde(default)]
    pub messages: Vec<ContextMessage>,
    /// Provider instance for which the mutable model context is normalized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub usage: SessionUsage,
    #[serde(default)]
    pub compaction: CompactionState,
    #[serde(default)]
    pub env_watcher: EnvWatcherState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_sources: Vec<RuleSource>,
}

impl SessionContext {
    pub fn with_system_prompt(system_prompt: SystemPromptBlock) -> Self {
        let baseline = system_prompt
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.dynamic());
        Self {
            messages: vec![ContextMessage::system(system_prompt.content.clone())],
            env_watcher: EnvWatcherState { baseline },
            system_prompt,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextManager {
    context: SessionContext,
    message_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PendingContextMessage {
    User(MessageContent),
    Internal(MessageContent),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingMessageBatch {
    pub messages: Vec<PendingContextMessage>,
    pub should_continue: bool,
}

impl ContextManager {
    pub fn new(context: SessionContext) -> Self {
        let mut context = context;
        if context.system_prompt.is_initialized()
            && context
                .messages
                .first()
                .is_none_or(|message| message.role != crate::MessageRole::System)
        {
            context.messages.insert(
                0,
                ContextMessage::system(context.system_prompt.content.clone()),
            );
        }
        let message_tokens = context
            .messages
            .iter()
            .map(estimate_message_tokens)
            .fold(0, u64::saturating_add);
        Self {
            context,
            message_tokens,
        }
    }

    pub fn initialize(builder: &SystemPromptBuilder) -> Result<Self, PromptBuildError> {
        let mut context = SessionContext::with_system_prompt(builder.build_initial()?);
        context.rule_sources = builder.rule_sources().to_vec();
        Ok(Self::new(context))
    }

    pub fn context(&self) -> &SessionContext {
        &self.context
    }

    pub fn into_context(self) -> SessionContext {
        self.context
    }

    pub fn system_prompt(&self) -> &str {
        &self.context.system_prompt.content
    }

    pub fn model_messages(&self) -> &[ContextMessage] {
        &self.context.messages
    }

    pub fn contains_images(&self) -> bool {
        self.context
            .messages
            .iter()
            .any(|message| message.content.contains_images())
    }

    pub fn append_user(&mut self, content: impl Into<MessageContent>) {
        self.extend_messages([ContextMessage::user(content)]);
    }

    pub fn append_internal(&mut self, kind: MessageKind, content: impl Into<MessageContent>) {
        self.extend_messages([ContextMessage::internal(kind, content)]);
    }

    pub fn replace_plan_watcher(&mut self, content: Option<MessageContent>) -> bool {
        let watcher_count = self
            .context
            .messages
            .iter()
            .filter(|message| message.kind == MessageKind::PlanWatcher)
            .count();
        let already_current = match content.as_ref() {
            Some(content) => {
                watcher_count == 1
                    && self.context.messages.iter().any(|message| {
                        message.kind == MessageKind::PlanWatcher && &message.content == content
                    })
            }
            None => watcher_count == 0,
        };
        if already_current {
            return false;
        }
        self.context
            .messages
            .retain(|message| message.kind != MessageKind::PlanWatcher);
        if let Some(content) = content {
            self.context
                .messages
                .push(ContextMessage::internal(MessageKind::PlanWatcher, content));
        }
        self.recalculate_message_tokens();
        true
    }

    pub fn append_assistant(&mut self, content: impl Into<String>, tool_calls: Vec<Value>) {
        self.append_assistant_with_reasoning(content, None, tool_calls);
    }

    pub fn append_assistant_with_reasoning(
        &mut self,
        content: impl Into<String>,
        reasoning: Option<String>,
        tool_calls: Vec<Value>,
    ) {
        let content = content.into();
        self.extend_messages([ContextMessage::assistant_with_reasoning(
            content, reasoning, tool_calls,
        )]);
    }

    pub fn append_response_items(
        &mut self,
        provider: impl Into<String>,
        response_items: Vec<Value>,
    ) {
        let provider = provider.into();
        self.extend_messages(response_items.into_iter().map(|item| {
            let owner =
                ContextMessage::response_item_requires_provider(&item).then(|| provider.clone());
            ContextMessage::response_item(item, owner)
        }));
    }

    pub fn append_tool(&mut self, result: ToolResultRecord) {
        let messages = std::iter::once(ContextMessage::tool(&result))
            .chain(result.model_context.into_iter().map(ContextMessage::user));
        self.extend_messages(messages);
    }

    pub fn append_tool_batch(&mut self, results: impl IntoIterator<Item = ToolResultRecord>) {
        let results = results.into_iter().collect::<Vec<_>>();
        let mut messages = Vec::with_capacity(results.len());
        messages.extend(results.iter().map(ContextMessage::tool));
        messages.extend(
            results
                .into_iter()
                .flat_map(|result| result.model_context)
                .map(ContextMessage::user),
        );
        self.extend_messages(messages);
    }

    /// Removes a control-tool exchange from the model context while leaving
    /// the client transcript untouched.
    pub fn remove_tool_call(&mut self, tool_call_id: &str) {
        self.context.messages.retain(|message| {
            message.tool_call_id.as_deref() != Some(tool_call_id)
                && !message.calls_tool(tool_call_id)
        });
        self.recalculate_message_tokens();
    }

    pub fn append_pending(&mut self, batch: PendingMessageBatch) -> bool {
        for message in batch.messages {
            match message {
                PendingContextMessage::User(content) => {
                    self.append_user(content);
                }
                PendingContextMessage::Internal(content) => {
                    self.append_internal(MessageKind::Runtime, content);
                }
            }
        }
        batch.should_continue
    }

    pub fn record_model_success(&mut self, model: impl Into<String>) {
        self.context.usage.last_model = Some(model.into());
    }

    /// Permanently normalize stored model context for the selected provider and
    /// input capabilities. Returns whether any persisted item changed.
    pub fn normalize_for_selection(
        &mut self,
        provider: &str,
        assumed_source_provider: Option<&str>,
        allow_image_input: bool,
    ) -> bool {
        let previous_provider = self.context.provider.clone();
        let source_provider = self
            .context
            .provider
            .as_deref()
            .or(assumed_source_provider)
            .unwrap_or(provider)
            .to_string();
        let provider_changed = source_provider != provider;
        let mut changed = previous_provider.as_deref() != Some(provider);
        let mut messages = Vec::with_capacity(self.context.messages.len());
        for mut message in self.context.messages.drain(..) {
            if message.provider.is_none()
                && message
                    .response_item_value()
                    .is_some_and(ContextMessage::response_item_requires_provider)
            {
                message.provider = Some(source_provider.clone());
                changed = true;
            }
            if provider_changed && message.is_provider_owned() {
                changed = true;
                continue;
            }
            let projected = message.project_for_image_input(allow_image_input);
            changed |= projected.as_ref() != Some(&message);
            messages.extend(projected);
        }
        self.context.messages = messages;
        self.context.provider = Some(provider.to_string());
        if changed {
            self.recalculate_message_tokens();
        }
        changed
    }

    pub fn refresh_usage(&mut self, tools: &[Value]) -> u64 {
        let tokens = self
            .message_tokens
            .saturating_add(estimate_tool_tokens(tools));
        self.context.usage.current_tokens = tokens;
        tokens
    }

    /// Refresh derived usage and return a persistence-ready context snapshot.
    pub fn checkpoint(&mut self, tools: &[Value]) -> SessionContext {
        self.refresh_usage(tools);
        self.context.clone()
    }

    fn should_compact(&self, trigger_tokens: u64) -> bool {
        trigger_tokens > 0 && self.context.usage.current_tokens >= trigger_tokens
    }

    /// Refresh the complete request estimate and plan scheduled compaction when needed.
    pub fn scheduled_compaction(
        &mut self,
        trigger_tokens: u64,
        tools: &[Value],
    ) -> Option<CompactionPlan> {
        self.refresh_usage(tools);
        self.should_compact(trigger_tokens)
            .then(|| CompactionPlanner::default().build(&self.context))
    }

    /// Scan mutable profile/environment state at a model-step boundary.
    pub fn refresh_environment(
        &mut self,
        builder: &SystemPromptBuilder,
    ) -> Result<usize, PromptBuildError> {
        let current = builder.scan_dynamic()?;
        Ok(self.refresh_environment_snapshot(current))
    }

    pub fn refresh_environment_snapshot(&mut self, current: DynamicEnvironmentSnapshot) -> usize {
        let changes = self.context.env_watcher.update(current);
        let count = changes.len();
        self.extend_messages(
            changes
                .into_iter()
                .map(|change| ContextMessage::internal(MessageKind::EnvWatcher, change.render())),
        );
        count
    }

    pub fn plan_compaction(&self, planner: &CompactionPlanner) -> CompactionPlan {
        planner.build(&self.context)
    }

    pub fn plan_recovery_compaction(&self, planner: &CompactionPlanner) -> CompactionPlan {
        planner.build_recovery(&self.context)
    }

    pub fn recovery_compaction(&self) -> CompactionPlan {
        CompactionPlanner::default().build_recovery(&self.context)
    }

    /// Replace model context and estimate the complete rebuilt request.
    pub fn apply_compaction(
        &mut self,
        plan: CompactionPlan,
        summary: impl Into<String>,
        prompt_builder: &SystemPromptBuilder,
        tools: &[Value],
    ) -> Result<(), PromptBuildError> {
        let summary = summary.into();
        let rebuilt_prompt = prompt_builder.rebuild()?;
        let baseline = rebuilt_prompt
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.dynamic());
        let replacement = plan.into_replacement(&rebuilt_prompt, summary.clone());
        self.context.system_prompt = rebuilt_prompt;
        self.context.messages = replacement;
        self.recalculate_message_tokens();
        self.context.env_watcher = EnvWatcherState { baseline };
        self.context.compaction.count = self.context.compaction.count.saturating_add(1);
        self.context.compaction.summary = (!summary.is_empty()).then_some(summary);
        self.refresh_usage(tools);
        Ok(())
    }

    fn extend_messages(&mut self, messages: impl IntoIterator<Item = ContextMessage>) {
        for message in messages {
            self.message_tokens = self
                .message_tokens
                .saturating_add(estimate_message_tokens(&message));
            self.context.messages.push(message);
        }
    }

    fn recalculate_message_tokens(&mut self) {
        self.message_tokens = self
            .context
            .messages
            .iter()
            .map(estimate_message_tokens)
            .fold(0, u64::saturating_add);
    }
}
