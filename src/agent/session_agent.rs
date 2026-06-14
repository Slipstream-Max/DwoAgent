//! Single-session agent runtime behavior.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::sync::Mutex;

use super::activity::{ActivityTurnHandle, SessionActivity};
use super::constants::parse_policy_mode;
use super::factory::RuntimeFactoryBuilder;
use super::session::{SESSION_TITLE_LENGTH, Session, SessionPersistence};
use super::turn::{PolicyModeGetter, TurnRuntime, run_turn};
use crate::config::loader::utc_iso;
use crate::config::models::{
    AgentState, ContextUsageSnapshot, ModelProfile, PolicyMode, ReasoningMode, SessionMetaPayload,
    StopReason,
};
use crate::config::policy::ToolPolicyConfig;
use crate::context::manager::{
    CancelEvent, CompactionOutcome, ConversationContextManager, SystemMessagesBuilder,
};
use crate::llm::client::{BaseLlmClient, LlmRequestOptions};
use crate::tools::subagent_tool_runtime::{PermissionRequester, UpdateEmitter};
use crate::tools::tool_run_manager::ToolRunManager;
use crate::utils::prompt::extract_first_text;
use crate::watchers::runtime::WatcherRuntime;

pub struct SessionAgent {
    /// Immutable session identifier cached on the outside of the mutex so
    /// synchronous accessors don't need to lock.
    session_id_cached: String,
    cwd_cached: String,
    max_running_turn_cached: Option<u32>,
    session_dir_cached: PathBuf,
    session: Arc<Mutex<Session>>,
    activity: SessionActivity,
    runtime: Mutex<RuntimeState>,
    persistence: Arc<SessionPersistence>,
    model_profiles: Mutex<HashMap<String, ModelProfile>>,
    runtime_factory_builder: RuntimeFactoryBuilder,
    cancel_event: CancelEvent,
    tool_manager_cached: Arc<ToolRunManager>,
    tool_schemas_cached: Arc<Vec<Value>>,
    tool_policy: Arc<ToolPolicyConfig>,
    watcher_runtime: Option<Arc<WatcherRuntime>>,
    prompt_lock: Mutex<()>,
}

struct RuntimeState {
    model_client: BaseLlmClient,
    tool_manager: Arc<ToolRunManager>,
    context_manager: ConversationContextManager,
}

impl SessionAgent {
    pub fn new(
        session: Session,
        model_client: BaseLlmClient,
        tool_manager: Arc<ToolRunManager>,
        context_manager: ConversationContextManager,
        model_profiles: HashMap<String, ModelProfile>,
        runtime_factory_builder: RuntimeFactoryBuilder,
        watcher_runtime: Option<Arc<WatcherRuntime>>,
        tool_policy: Arc<ToolPolicyConfig>,
    ) -> Arc<Self> {
        let session_id_cached = session.session_id.clone();
        let cwd_cached = session.cwd.clone();
        let max_running_turn_cached = session.max_running_turn;
        let session_dir_cached = session.session_dir.clone();
        let tool_schemas_cached = Arc::new(session.tool_schemas.clone());
        let persistence = Arc::new(SessionPersistence::new(session.session_dir.clone()));
        let session = Arc::new(Mutex::new(session));
        let activity = SessionActivity::new(
            session_id_cached.clone(),
            session.clone(),
            persistence.clone(),
        );
        Arc::new(Self {
            session_id_cached,
            cwd_cached,
            max_running_turn_cached,
            session_dir_cached,
            activity,
            session,
            runtime: Mutex::new(RuntimeState {
                model_client,
                tool_manager: tool_manager.clone(),
                context_manager,
            }),
            persistence,
            model_profiles: Mutex::new(model_profiles),
            runtime_factory_builder,
            cancel_event: CancelEvent::new(),
            tool_manager_cached: tool_manager.clone(),
            tool_schemas_cached,
            tool_policy,
            watcher_runtime,
            prompt_lock: Mutex::new(()),
        })
    }

    // ── Lightweight accessors ────────────────────────────────────────────

    pub fn session_id(&self) -> &str {
        &self.session_id_cached
    }

    pub async fn session_snapshot(&self) -> Session {
        self.session.lock().await.clone()
    }

    pub async fn session_meta_snapshot(&self) -> SessionMetaPayload {
        self.session.lock().await.to_meta_payload()
    }

    pub async fn context_manager_snapshot(&self) -> Vec<Value> {
        self.runtime
            .lock()
            .await
            .context_manager
            .messages()
            .to_vec()
    }

    pub async fn context_usage_snapshot(&self) -> ContextUsageSnapshot {
        self.runtime.lock().await.context_manager.usage_snapshot()
    }

    pub async fn tool_manager(&self) -> Arc<ToolRunManager> {
        self.tool_manager_cached.clone()
    }

    pub fn cancel_event(&self) -> CancelEvent {
        self.cancel_event.clone()
    }

    pub fn cwd(&self) -> &str {
        &self.cwd_cached
    }

    pub fn max_running_turn(&self) -> Option<u32> {
        self.max_running_turn_cached
    }

    pub fn session_dir(&self) -> &std::path::Path {
        &self.session_dir_cached
    }

    pub async fn state(&self) -> AgentState {
        self.session.lock().await.state
    }

    pub async fn title(&self) -> Option<String> {
        self.session.lock().await.title.clone()
    }

    pub async fn updated_at(&self) -> Option<String> {
        self.session.lock().await.updated_at.clone()
    }

    pub async fn is_active(&self) -> bool {
        matches!(
            self.session.lock().await.state,
            AgentState::Running | AgentState::WaitingUserConfirm | AgentState::Cancelling
        )
    }

    // ── Core lifecycle ───────────────────────────────────────────────────

    pub async fn run_prompt(
        self: Arc<Self>,
        user_input: Value,
        user_blocks: Vec<Value>,
        emit_update: UpdateEmitter,
        request_permission: PermissionRequester,
        extra_tool_schemas: Vec<Value>,
    ) -> Result<String> {
        if self.is_active().await {
            self.cancel().await;
        }

        let _guard = self.prompt_lock.lock().await;
        if self.is_active().await {
            self.set_stop_internal("cancelled").await?;
        }

        self.apply_pending_config_if_needed().await?;
        self.cancel_event.clear();
        self.set_state_internal(AgentState::Running).await?;
        {
            let mut session = self.session.lock().await;
            session.stop_reason = None;
        }
        let activity = self.activity.bind_turn(
            emit_update.clone(),
            request_permission.clone(),
            self.cancel_event.clone(),
        );

        // Set a cheap fallback immediately, then refine it with a background
        // model request so title generation does not block the main turn.
        {
            let mut session = self.session.lock().await;
            if session.title.is_none()
                && let Some(source_text) = extract_session_title_source(&user_input, &user_blocks)
            {
                let fallback_title = fallback_session_title(&source_text);
                session.title = Some(fallback_title.clone());
                let (messages, usage) = {
                    let runtime = self.runtime.lock().await;
                    (
                        runtime.context_manager.messages().to_vec(),
                        runtime.context_manager.usage_snapshot(),
                    )
                };
                self.persistence.save_session(&session, &messages, usage)?;
                let updated_at = session.updated_at.clone();
                drop(session);
                let _ = activity
                    .session_info_update(&fallback_title, updated_at.as_deref())
                    .await;
                self.spawn_title_refinement(source_text, fallback_title, activity.clone());
            }
        }

        self.persist_async().await?;

        self.activity
            .record_user_input(&user_input, &user_blocks)
            .await?;
        self.persist_async().await?;

        // Assemble the turn runtime. Reasoning mode / model client are
        // snapshotted here so in-flight mutation doesn't desynchronise.
        // Policy mode is deliberately live: permission checks read the latest
        // session setting so ACP runtime changes apply to the next tool call.
        let get_policy_mode_for_turn: PolicyModeGetter = Arc::new({
            let this = self.clone();
            move || {
                let this = this.clone();
                Box::pin(async move { this.session.lock().await.mode_id.as_str().to_string() })
            }
        });
        let reasoning_for_turn = self
            .session
            .lock()
            .await
            .reasoning_mode
            .as_str()
            .to_string();
        let max_running_turn = self.max_running_turn();

        let tool_manager_ref = self.runtime.lock().await.tool_manager.clone();
        let request_tool_schemas =
            effective_tool_schemas(self.tool_schemas_cached.clone(), extra_tool_schemas);
        let rebuild_builder: Option<SystemMessagesBuilder> = Some(Arc::new({
            let this = self.clone();
            move || {
                let this = this.clone();
                Box::pin(async move { this.rebuild_system_messages().await })
            }
        }));

        let turn_result = {
            let mut runtime_guard = self.runtime.lock().await;
            let rt_parts = &mut *runtime_guard;
            let turn_runtime = TurnRuntime {
                user_input: user_input.clone(),
                get_policy_mode: get_policy_mode_for_turn,
                max_running_turn,
                activity: activity.clone(),
                reasoning_mode: reasoning_for_turn,
                model_client: &rt_parts.model_client,
                tool_schemas: request_tool_schemas,
                tool_policy: self.tool_policy.clone(),
                tool_manager: &tool_manager_ref,
                context_manager: &mut rt_parts.context_manager,
                rebuild_system_messages: rebuild_builder,
                watcher_runtime: self.watcher_runtime.clone(),
            };
            run_turn(turn_runtime).await
        };

        match turn_result {
            Ok(result) => {
                self.set_stop_internal(&result.stop_reason).await?;
                self.apply_pending_config_if_needed().await?;
                self.persist_async().await?;
                Ok(result.stop_reason)
            }
            Err(err) => {
                self.set_stop_internal("completed").await?;
                Err(err)
            }
        }
    }

    pub async fn cancel(&self) {
        if !self.is_active().await {
            return;
        }
        self.cancel_event.set();
        let tool_manager = self.tool_manager_cached.clone();
        tokio::spawn(async move {
            tool_manager.cancel_running_tools().await;
        });
        {
            let mut session = self.session.lock().await;
            session.state = AgentState::Cancelling;
            session.updated_at = Some(utc_iso());
        }
        let _ = self.persist_session_meta_async().await;
    }

    pub async fn cancel_tool_call(&self, tool_call_id: &str) -> bool {
        self.tool_manager_cached
            .cancel_tool_call(tool_call_id)
            .await
    }

    pub async fn mark_loaded(&self, reset_active: bool) -> Result<()> {
        {
            let mut session = self.session.lock().await;
            session.updated_at = Some(utc_iso());
            if reset_active
                || !matches!(
                    session.state,
                    AgentState::Running | AgentState::WaitingUserConfirm | AgentState::Cancelling
                )
            {
                session.state = AgentState::Idle;
            }
        }
        self.persist_async().await
    }

    pub async fn set_mode(&self, mode_id: &str) -> Result<()> {
        let text = parse_policy_mode(mode_id)?;
        let mode = PolicyMode::from_str(&text)?;
        {
            let mut session = self.session.lock().await;
            session.mode_id = mode;
            session.updated_at = Some(utc_iso());
        }
        if self.is_active().await {
            self.persist_session_meta_async().await?;
        } else {
            self.persist_async().await?;
        }
        Ok(())
    }

    pub async fn set_model(&self, model_id: &str) -> Result<&'static str> {
        {
            let profiles = self.model_profiles.lock().await;
            if !profiles.contains_key(model_id) {
                bail!("Unknown model_id: {model_id}");
            }
        }
        if self.is_active().await {
            let mut session = self.session.lock().await;
            session.pending_model_id = Some(model_id.to_string());
            session.updated_at = Some(utc_iso());
            drop(session);
            self.persist_session_meta_async().await?;
            return Ok("queued");
        }
        self.apply_model_switch(model_id).await?;
        let _ = self.compact_if_over_context_window().await?;
        self.persist_async().await?;
        Ok("applied")
    }

    pub async fn set_reasoning_mode(&self, reasoning_mode: &str) -> Result<&'static str> {
        let mode_enum = ReasoningMode::from_str(reasoning_mode)?;
        let model_id = {
            let session = self.session.lock().await;
            session
                .pending_model_id
                .clone()
                .unwrap_or_else(|| session.model_id.clone())
        };
        let profile = {
            let profiles = self.model_profiles.lock().await;
            profiles
                .get(model_id.as_str())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {model_id}"))?
        };
        if !profile.reasoning_modes.contains(&mode_enum) {
            bail!("Unknown reasoning_mode `{reasoning_mode}` for model `{model_id}`");
        }
        if self.is_active().await {
            let mut session = self.session.lock().await;
            session.pending_reasoning_mode = Some(mode_enum);
            session.updated_at = Some(utc_iso());
            drop(session);
            self.persist_session_meta_async().await?;
            return Ok("queued");
        }
        {
            let mut session = self.session.lock().await;
            session.reasoning_mode = mode_enum;
            session.updated_at = Some(utc_iso());
        }
        self.persist_async().await?;
        Ok("applied")
    }

    async fn set_state_internal(&self, state: AgentState) -> Result<()> {
        {
            let mut session = self.session.lock().await;
            session.state = state;
            session.updated_at = Some(utc_iso());
        }
        self.persist_async().await?;
        Ok(())
    }

    async fn set_stop_internal(&self, stop_reason: &str) -> Result<()> {
        let stop_enum = StopReason::from_str(stop_reason).ok();
        {
            let mut session = self.session.lock().await;
            session.state = AgentState::Stop;
            session.stop_reason = stop_enum;
            session.updated_at = Some(utc_iso());
        }
        self.persist_async().await?;
        Ok(())
    }

    async fn apply_pending_model_switch_if_needed(&self) -> Result<bool> {
        let pending = {
            let session = self.session.lock().await;
            session.pending_model_id.clone()
        };
        let Some(model_id) = pending else {
            return Ok(false);
        };
        self.apply_model_switch(&model_id).await?;
        {
            let mut session = self.session.lock().await;
            session.pending_model_id = None;
        }
        Ok(true)
    }

    async fn apply_pending_reasoning_mode_if_needed(&self) -> Result<()> {
        let (pending, model_id) = {
            let session = self.session.lock().await;
            (session.pending_reasoning_mode, session.model_id.clone())
        };
        let Some(pending) = pending else {
            return Ok(());
        };
        // Clone the profile out while holding only the profiles lock so we
        // never hold `model_profiles` and `session` simultaneously. This
        // mirrors `apply_model_switch` and avoids the ABBA with
        // `set_reasoning_mode`.
        let profile = {
            let profiles = self.model_profiles.lock().await;
            profiles
                .get(model_id.as_str())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {model_id}"))?
        };
        if !profile.reasoning_modes.contains(&pending) {
            bail!(
                "Unknown reasoning_mode `{}` for model `{model_id}`",
                pending.as_str()
            );
        }
        {
            let mut session = self.session.lock().await;
            session.reasoning_mode = pending;
            session.pending_reasoning_mode = None;
        }
        Ok(())
    }

    async fn apply_pending_config_if_needed(&self) -> Result<()> {
        let model_switched = self.apply_pending_model_switch_if_needed().await?;
        self.apply_pending_reasoning_mode_if_needed().await?;
        if model_switched {
            let _ = self.compact_if_over_context_window().await?;
        }
        Ok(())
    }

    async fn apply_model_switch(&self, model_id: &str) -> Result<()> {
        let profile = {
            let profiles = self.model_profiles.lock().await;
            profiles
                .get(model_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {model_id}"))?
        };
        let cwd = {
            let session = self.session.lock().await;
            session.cwd.clone()
        };
        let factory = (self.runtime_factory_builder)(&cwd, &profile)?;
        let new_client = factory.create_model_client()?;
        let (current_messages, current_usage) = {
            let runtime = self.runtime.lock().await;
            (
                runtime.context_manager.messages().to_vec(),
                runtime.context_manager.usage_snapshot(),
            )
        };
        let mut new_context = factory.rebuild_context_manager(current_messages)?;
        new_context.sync_token_usage(current_usage.used);

        {
            let mut runtime = self.runtime.lock().await;
            runtime.model_client = new_client;
            runtime.context_manager = new_context;
        }
        {
            let mut session = self.session.lock().await;
            session.model_id = model_id.to_string();
            if !profile.reasoning_modes.contains(&session.reasoning_mode) {
                session.reasoning_mode = profile.default_reasoning_mode;
            }
            if let Some(pending) = session.pending_reasoning_mode
                && !profile.reasoning_modes.contains(&pending)
            {
                session.pending_reasoning_mode = None;
            }
            session.updated_at = Some(utc_iso());
        }
        Ok(())
    }

    async fn compact_if_over_context_window(&self) -> Result<CompactionOutcome> {
        let over_window = {
            let runtime = self.runtime.lock().await;
            runtime.context_manager.is_over_context_window()
        };
        if !over_window {
            return Ok(CompactionOutcome::Skipped);
        }

        let reasoning_mode = {
            self.session
                .lock()
                .await
                .reasoning_mode
                .as_str()
                .to_string()
        };
        let mut runtime = self.runtime.lock().await;
        let RuntimeState {
            model_client,
            context_manager,
            ..
        } = &mut *runtime;
        Ok(context_manager
            .maybe_compact(
                model_client,
                None,
                None,
                Some(&reasoning_mode),
                LlmRequestOptions::default(),
            )
            .await)
    }

    async fn rebuild_system_messages(&self) -> Result<Vec<Value>> {
        let (cwd, model_id) = {
            let session = self.session.lock().await;
            (session.cwd.clone(), session.model_id.clone())
        };
        let profile = {
            let profiles = self.model_profiles.lock().await;
            profiles
                .get(&model_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {model_id}"))?
        };
        let factory = (self.runtime_factory_builder)(&cwd, &profile)?;
        factory.rebuild_system_messages()
    }

    pub async fn persist_async(&self) -> Result<()> {
        let session = self.session.lock().await.clone();
        let (messages, usage) = {
            let runtime = self.runtime.lock().await;
            (
                runtime.context_manager.messages().to_vec(),
                runtime.context_manager.usage_snapshot(),
            )
        };
        self.persistence.save_session(&session, &messages, usage)
    }

    async fn persist_session_meta_async(&self) -> Result<()> {
        let session = self.session.lock().await.clone();
        self.persistence.save_session_meta(&session)
    }

    fn spawn_title_refinement(
        self: &Arc<Self>,
        source_text: String,
        fallback_title: String,
        activity: ActivityTurnHandle,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let Ok(Some(title)) = this.generate_session_title(&source_text).await else {
                return;
            };
            if title == fallback_title {
                return;
            }

            let (session_snapshot, updated_at) = {
                let mut session = this.session.lock().await;
                if session.title.as_deref() != Some(fallback_title.as_str()) {
                    return;
                }
                session.title = Some(title.clone());
                session.updated_at = Some(utc_iso());
                (session.clone(), session.updated_at.clone())
            };

            if this
                .persistence
                .save_session_meta(&session_snapshot)
                .is_err()
            {
                return;
            }
            let _ = activity
                .session_info_update(&title, updated_at.as_deref())
                .await;
        });
    }

    async fn generate_session_title(&self, source_text: &str) -> Result<Option<String>> {
        let (cwd, model_id) = {
            let session = self.session.lock().await;
            (session.cwd.clone(), session.model_id.clone())
        };
        let profile = {
            let profiles = self.model_profiles.lock().await;
            profiles
                .get(&model_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown model_id: {model_id}"))?
        };
        let client = (self.runtime_factory_builder)(&cwd, &profile)?
            .create_model_client()
            .context("create title model client")?;
        let title_source = title_source_excerpt(source_text);
        let messages = vec![
            serde_json::json!({
                "role": "system",
                "content": "You generate concise conversation titles. Return only the title, with no quotes or explanation. Use the same language as the user's message. Keep it under 12 Chinese characters or 6 English words."
            }),
            serde_json::json!({
                "role": "user",
                "content": format!("User message:\n{title_source}\n\nTitle:")
            }),
        ];
        let mut options = LlmRequestOptions::default();
        options.retry.request_max_retries = 0;
        let response = client
            .request_with_usage(&messages, None, None, None, options)
            .await?;
        let content = response
            .message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("");
        Ok(clean_generated_session_title(content))
    }
}

fn effective_tool_schemas(base: Arc<Vec<Value>>, extra: Vec<Value>) -> Arc<Vec<Value>> {
    if extra.is_empty() {
        return base;
    }

    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(base.len() + extra.len());
    for schema in base.iter().cloned() {
        if let Some(name) = tool_schema_name(&schema) {
            seen.insert(name);
        }
        merged.push(schema);
    }
    for schema in extra {
        match tool_schema_name(&schema) {
            Some(name) => {
                if seen.insert(name) {
                    merged.push(schema);
                }
            }
            None => merged.push(schema),
        }
    }
    Arc::new(merged)
}

fn tool_schema_name(schema: &Value) -> Option<String> {
    schema
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_session_title_source(user_input: &Value, user_blocks: &[Value]) -> Option<String> {
    extract_first_text(user_input)
        .or_else(|| extract_first_text(&Value::Array(user_blocks.to_vec())))
}

fn fallback_session_title(text: &str) -> String {
    text.chars().take(SESSION_TITLE_LENGTH).collect()
}

fn title_source_excerpt(text: &str) -> String {
    const TITLE_SOURCE_CHAR_LIMIT: usize = 2000;
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index == TITLE_SOURCE_CHAR_LIMIT {
            return out;
        }
        out.push(ch);
    }
    out
}

fn clean_generated_session_title(raw: &str) -> Option<String> {
    const GENERATED_TITLE_LENGTH: usize = 40;

    let first_line = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    let title = first_line
        .strip_prefix("Title:")
        .or_else(|| first_line.strip_prefix("title:"))
        .or_else(|| first_line.strip_prefix("标题:"))
        .or_else(|| first_line.strip_prefix("标题："))
        .unwrap_or(first_line)
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .trim()
        .trim_end_matches(|ch| matches!(ch, '.' | '。'))
        .trim();
    if title.is_empty() {
        return None;
    }
    Some(title.chars().take(GENERATED_TITLE_LENGTH).collect())
}
