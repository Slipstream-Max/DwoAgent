//! Factory for reusable agent runtime components.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use super::session::Session;
use super::session_agent::SessionAgent;
use crate::config::loader::utc_iso;
use crate::config::models::{
    AgentState, AgentTools, ModelCapabilities, ModelConfig, ModelProfile, PolicyMode,
    ReasoningMode, StopReason,
};
use crate::context::builder::build_agent_system_context;
use crate::context::manager::ConversationContextManager;
use crate::llm::client::{BaseLlmClient, create_model_client};
use crate::tools::tool_run_manager::ToolRunManager;

/// Bundle of runtime parts created together.
pub struct AgentRuntimeParts {
    pub model_client: BaseLlmClient,
    pub tool_manager: Arc<ToolRunManager>,
    pub context_manager: ConversationContextManager,
}

/// Strategy hook used for rebuilding the factory on model switches.
pub type RuntimeFactoryBuilder =
    Arc<dyn Fn(&str, &ModelProfile) -> Result<SessionAgentFactory> + Send + Sync>;

/// Initialize model, tool, context, and subagent runtime wiring.
pub struct SessionAgentFactory {
    agent_structure_dir: PathBuf,
    agent_id: String,
    mcp_config_path: PathBuf,
    cwd: String,
    context_window_tokens: u32,
    compact_threshold: f64,
    model_config: ModelConfig,
    model_capabilities: ModelCapabilities,
    default_reasoning_mode: String,
    runtime_tools: AgentTools,
}

/// Shape of the arguments passed to `SessionAgentFactory::create_session_agent`.
pub struct CreateSessionArgs {
    pub session_id: String,
    pub cwd: String,
    pub session_dir: PathBuf,
    pub model_id: String,
    pub model_profiles: HashMap<String, ModelProfile>,
    pub runtime_factory_builder: RuntimeFactoryBuilder,
    pub max_running_turn: Option<u32>,
    pub runtime_tools: AgentTools,
    pub mode_id: PolicyMode,
    pub state: AgentState,
    pub stop_reason: Option<StopReason>,
    pub title: Option<String>,
    pub context_messages: Option<Vec<Value>>,
    pub transcript_events: Vec<Value>,
    pub pending_model_id: Option<String>,
    pub reasoning_mode: Option<ReasoningMode>,
    pub pending_reasoning_mode: Option<ReasoningMode>,
    pub tool_schemas: Vec<Value>,
}

impl SessionAgentFactory {
    pub fn new(
        agent_structure_dir: &Path,
        agent_id: impl Into<String>,
        mcp_config_path: &Path,
        cwd: impl Into<String>,
        context_window_tokens: u32,
        compact_threshold: f64,
        model_config: ModelConfig,
        model_capabilities: ModelCapabilities,
        default_reasoning_mode: impl Into<String>,
        runtime_tools: AgentTools,
    ) -> Self {
        let structure = std::fs::canonicalize(agent_structure_dir)
            .unwrap_or_else(|_| agent_structure_dir.to_path_buf());
        let mcp = std::fs::canonicalize(mcp_config_path)
            .unwrap_or_else(|_| mcp_config_path.to_path_buf());
        let cwd_text = cwd.into();
        let cwd_resolved = std::fs::canonicalize(&cwd_text)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(cwd_text);
        Self {
            agent_structure_dir: structure,
            agent_id: agent_id.into(),
            mcp_config_path: mcp,
            cwd: cwd_resolved,
            context_window_tokens,
            compact_threshold,
            model_config,
            model_capabilities,
            default_reasoning_mode: default_reasoning_mode.into(),
            runtime_tools,
        }
    }

    pub fn mcp_config_path(&self) -> &Path {
        &self.mcp_config_path
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn create_model_client(&self) -> Result<BaseLlmClient> {
        create_model_client(
            self.model_config.clone(),
            self.model_capabilities,
            &self.default_reasoning_mode,
        )
    }

    pub async fn create_tool_manager(&self) -> Result<Arc<ToolRunManager>> {
        let cwd_path = PathBuf::from(&self.cwd);
        let mcp_path: Option<&Path> =
            if self.mcp_config_path.as_os_str().is_empty() || !self.runtime_tools.mcp_enabled() {
                None
            } else {
                Some(self.mcp_config_path.as_path())
            };
        let manager =
            ToolRunManager::new(mcp_path, Some(cwd_path.as_path()), 300, self.runtime_tools)
                .await?;
        Ok(Arc::new(manager))
    }

    pub fn create_context_manager(
        &self,
        init_messages: Vec<Value>,
    ) -> Result<ConversationContextManager> {
        ConversationContextManager::new(
            init_messages,
            self.context_window_tokens,
            self.compact_threshold,
        )
    }

    pub fn rebuild_context_manager(
        &self,
        messages: Vec<Value>,
    ) -> Result<ConversationContextManager> {
        self.create_context_manager(messages)
    }

    pub async fn create_parts(&self, init_messages: Vec<Value>) -> Result<AgentRuntimeParts> {
        let tool_manager = self.create_tool_manager().await?;
        let model_client = self.create_model_client()?;
        let context_manager = self.create_context_manager(init_messages)?;
        Ok(AgentRuntimeParts {
            model_client,
            tool_manager,
            context_manager,
        })
    }

    pub async fn create_main_parts(
        &self,
        context_messages: Option<Vec<Value>>,
    ) -> Result<AgentRuntimeParts> {
        let tool_manager = self.create_tool_manager().await?;
        let init_messages = match context_messages {
            Some(messages) => messages,
            None => vec![self.build_system_context(tool_manager.mcp_server_names())?],
        };
        let context_manager = self.create_context_manager(init_messages)?;
        let model_client = self.create_model_client()?;
        Ok(AgentRuntimeParts {
            model_client,
            tool_manager,
            context_manager,
        })
    }

    pub async fn create_session_agent(self, args: CreateSessionArgs) -> Result<Arc<SessionAgent>> {
        let profile = args
            .model_profiles
            .get(&args.model_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {}", args.model_id))?;
        let parts = self.create_main_parts(args.context_messages).await?;
        let session = Session {
            session_id: args.session_id.clone(),
            cwd: args.cwd.clone(),
            session_dir: args.session_dir.clone(),
            model_id: args.model_id.clone(),
            max_running_turn: args.max_running_turn,
            runtime_tools: args.runtime_tools,
            tool_schemas: args.tool_schemas,
            mode_id: args.mode_id,
            state: args.state,
            stop_reason: args.stop_reason,
            title: args.title,
            updated_at: Some(utc_iso()),
            pending_model_id: args.pending_model_id,
            reasoning_mode: args
                .reasoning_mode
                .unwrap_or(profile.default_reasoning_mode),
            pending_reasoning_mode: args.pending_reasoning_mode,
            transcript_events: args.transcript_events,
        };
        let agent = SessionAgent::new(
            session,
            parts.model_client,
            parts.tool_manager.clone(),
            parts.context_manager,
            args.model_profiles,
            args.runtime_factory_builder,
        );

        self.attach_subagent_runtime(&agent).await?;
        Ok(agent)
    }

    pub async fn attach_subagent_runtime(&self, agent: &Arc<SessionAgent>) -> Result<()> {
        if !self.runtime_tools.subagent_enabled() {
            return Ok(());
        }
        use super::subagent::SubagentExecutor;
        let session_id = agent.session_id().to_string();
        let main_system_message = agent
            .context_manager_snapshot()
            .await
            .first()
            .cloned()
            .unwrap_or(Value::Null);
        let max_running_turn = agent.max_running_turn();
        let executor = SubagentExecutor::new(
            session_id,
            main_system_message,
            max_running_turn,
            self.clone_shape(),
            self.runtime_tools,
        );
        agent
            .tool_manager()
            .await
            .set_subagent_executor(Some(Arc::new(executor)))
            .await;
        Ok(())
    }

    /// Clone just the shape (not the MCP client / tool manager) so subagent
    /// runs can build their own `AgentRuntimeParts`.
    pub(crate) fn clone_shape(&self) -> Self {
        Self {
            agent_structure_dir: self.agent_structure_dir.clone(),
            agent_id: self.agent_id.clone(),
            mcp_config_path: self.mcp_config_path.clone(),
            cwd: self.cwd.clone(),
            context_window_tokens: self.context_window_tokens,
            compact_threshold: self.compact_threshold,
            model_config: self.model_config.clone(),
            model_capabilities: self.model_capabilities,
            default_reasoning_mode: self.default_reasoning_mode.clone(),
            runtime_tools: self.runtime_tools,
        }
    }

    pub async fn create_subagent_parts(
        &self,
        main_system_message: Value,
    ) -> Result<AgentRuntimeParts> {
        let subagent_system = json!({
            "role": "system",
            "content": "You are a temporary subagent.\n\
                       Only solve the assigned task.\n\
                       Use only the available tools.\n\
                       Return a final response for the parent agent.",
        });
        let init_messages = vec![main_system_message, subagent_system];
        self.create_parts(init_messages).await
    }

    pub fn build_system_context(&self, mcp_server_names: &[String]) -> Result<Value> {
        build_agent_system_context(
            &self.agent_structure_dir,
            &self.agent_id,
            &self.cwd,
            mcp_server_names,
            &self.runtime_tools,
        )
    }

    pub fn rebuild_system_messages(&self, mcp_server_names: &[String]) -> Result<Vec<Value>> {
        Ok(vec![self.build_system_context(mcp_server_names)?])
    }
}
