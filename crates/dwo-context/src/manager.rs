use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compaction::{CompactionPlan, CompactionPlanner};
use crate::env_watcher::EnvWatcherState;
use crate::prompt::{PromptBuildError, SystemPromptBlock, SystemPromptBuilder};
use crate::{
    ContextMessage, MessageContent, MessageKind, ToolResultRecord, TurnId, estimate_context_tokens,
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
    #[serde(default)]
    pub usage: SessionUsage,
    #[serde(default)]
    pub compaction: CompactionState,
    #[serde(default)]
    pub env_watcher: EnvWatcherState,
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
        Self { context }
    }

    pub fn initialize(builder: &SystemPromptBuilder) -> Result<Self, PromptBuildError> {
        Ok(Self::new(SessionContext::with_system_prompt(
            builder.build_initial()?,
        )))
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

    pub fn append_user(&mut self, _turn_id: TurnId, content: impl Into<MessageContent>) {
        self.context.messages.push(ContextMessage::user(content));
    }

    pub fn append_internal(&mut self, kind: MessageKind, content: impl Into<MessageContent>) {
        self.context
            .messages
            .push(ContextMessage::internal(kind, content));
    }

    pub fn append_assistant(
        &mut self,
        turn_id: TurnId,
        content: impl Into<String>,
        tool_calls: Vec<Value>,
    ) {
        self.append_assistant_with_reasoning(turn_id, content, None, tool_calls);
    }

    pub fn append_assistant_with_reasoning(
        &mut self,
        _turn_id: TurnId,
        content: impl Into<String>,
        reasoning: Option<String>,
        tool_calls: Vec<Value>,
    ) {
        let content = content.into();
        self.context
            .messages
            .push(ContextMessage::assistant_with_reasoning(
                content, reasoning, tool_calls,
            ));
    }

    pub fn append_tool(&mut self, _turn_id: TurnId, result: ToolResultRecord) {
        self.context.messages.push(ContextMessage::tool(&result));
    }

    pub fn record_model_success(&mut self, model: impl Into<String>) {
        self.context.usage.last_model = Some(model.into());
    }

    pub fn refresh_usage(&mut self, tools: &[Value]) -> u64 {
        let tokens = estimate_context_tokens(&self.context.messages, tools);
        self.context.usage.current_tokens = tokens;
        tokens
    }

    pub fn should_compact(&self, trigger_tokens: u64) -> bool {
        trigger_tokens > 0 && self.context.usage.current_tokens >= trigger_tokens
    }

    /// Scan mutable profile/environment state at a model-step boundary.
    pub fn refresh_environment(
        &mut self,
        builder: &SystemPromptBuilder,
    ) -> Result<usize, PromptBuildError> {
        let current = builder.scan_dynamic()?;
        let changes = self.context.env_watcher.update(current);
        let count = changes.len();
        self.context.messages.extend(
            changes
                .into_iter()
                .map(|change| ContextMessage::internal(MessageKind::EnvWatcher, change.render())),
        );
        Ok(count)
    }

    pub fn plan_compaction(&self, planner: &CompactionPlanner) -> CompactionPlan {
        planner.build(&self.context)
    }

    pub fn plan_recovery_compaction(&self, planner: &CompactionPlanner) -> CompactionPlan {
        planner.build_recovery(&self.context)
    }

    pub fn plan_image_downgrade(&self) -> CompactionPlan {
        CompactionPlanner::default().build_image_downgrade(&self.context)
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
        self.context.env_watcher = EnvWatcherState { baseline };
        self.context.compaction.count = self.context.compaction.count.saturating_add(1);
        self.context.compaction.summary = (!summary.is_empty()).then_some(summary);
        self.refresh_usage(tools);
        Ok(())
    }
}
