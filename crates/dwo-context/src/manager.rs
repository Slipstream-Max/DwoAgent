use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compaction::{CompactionPlan, CompactionPlanner};
use crate::env_watcher::EnvWatcherState;
use crate::prompt::{PromptBuildError, SystemPromptBlock, SystemPromptBuilder};
use crate::{ContextMessage, MessageContent, MessageKind, ToolResultRecord, TurnId};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Token count reported for the latest normal model response.
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

    pub fn append_user(&mut self, _turn_id: TurnId, content: impl Into<MessageContent>) {
        self.context.messages.push(ContextMessage::user(content));
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

    pub fn record_turn_usage(&mut self, model: impl Into<String>, total_tokens: u64) {
        self.context.usage.current_tokens = total_tokens;
        self.context.usage.last_model = Some(model.into());
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

    /// Replace model context and clear the reported token count until the next model response.
    pub fn apply_compaction(
        &mut self,
        plan: CompactionPlan,
        summary: impl Into<String>,
        prompt_builder: &SystemPromptBuilder,
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
        self.context.usage.current_tokens = 0;
        Ok(())
    }
}
