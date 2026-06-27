use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use super::builtin::subagent::{SubagentExecutor, ToolExecutionContext};
use super::builtin::terminal::TerminalExecutor;
use super::builtin::wait::{WaitTarget, parse_wait_target, wait_seconds, wait_session};
use super::session::{Cap, ToolSession};
use super::session_creator::SessionCreateRequest;
use super::tool_catalog::ToolSpec;
use super::tool_output::ToolOutput;

const SUBAGENT_NAMES: &[&str] = &[
    "alice", "bob", "claire", "david", "emma", "frank", "grace", "henry",
];

pub(crate) struct SessionManager {
    terminal_executor: TerminalExecutor,
    subagent_executor: Mutex<Option<Arc<dyn SubagentExecutor>>>,
    registry: SessionRegistry,
}

impl SessionManager {
    pub(crate) fn new(cwd: std::path::PathBuf, finished_ttl_seconds: u64) -> Self {
        Self {
            terminal_executor: TerminalExecutor::new(Some(cwd)),
            subagent_executor: Mutex::new(None),
            registry: SessionRegistry::new(finished_ttl_seconds),
        }
    }

    pub(crate) async fn set_subagent_executor(&self, executor: Option<Arc<dyn SubagentExecutor>>) {
        let mut guard = self.subagent_executor.lock().await;
        *guard = executor;
    }

    pub(crate) async fn shutdown(&self) {
        let sessions = self.registry.take_all_for_shutdown().await;
        for session in sessions {
            let mut guard = session.lock().await;
            let _ = guard.cancel().await;
        }
    }

    pub(crate) async fn cancel_running_tools(&self) {
        let sessions = self.registry.unique_sessions_and_clear_reservations().await;

        for session in sessions {
            let kind = {
                let guard = session.lock().await;
                guard
                    .list_item()
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            if kind == "subagent" {
                self.registry.touch(&session).await;
                continue;
            }
            self.registry.remove_aliases_for(&session).await;
            let mut guard = session.lock().await;
            let _ = guard.cancel().await;
        }
    }

    pub(crate) async fn cancel_tool_call(&self, tool_call_id: &str) -> bool {
        let session = self.registry.get(tool_call_id.trim()).await;
        match session {
            None => false,
            Some(session) => {
                {
                    let mut guard = session.lock().await;
                    let _ = guard.cancel().await;
                }
                self.registry.touch(&session).await;
                true
            }
        }
    }

    pub(crate) async fn is_closing(&self) -> bool {
        self.registry.is_closing().await
    }

    pub(crate) async fn prune_finished_sessions(&self) {
        self.registry.prune_finished().await;
    }

    pub(crate) async fn create_and_register(
        &self,
        tool_call_id: &str,
        name: &str,
        args: &Map<String, Value>,
        context: Option<&ToolExecutionContext>,
    ) -> Value {
        match self
            .try_create_and_register(tool_call_id, name, args, context)
            .await
        {
            Ok(value) => value,
            Err(err) => ToolOutput::error(name, format!("{err:#}")),
        }
    }

    async fn try_create_and_register(
        &self,
        tool_call_id: &str,
        name: &str,
        args: &Map<String, Value>,
        context: Option<&ToolExecutionContext>,
    ) -> Result<Value> {
        let request = SessionCreateRequest::parse(name, args, context)?;
        let session_kind = request.kind();
        let requested_name = request.requested_name().map(str::to_string);
        let session_name = self
            .registry
            .allocate_name(session_kind, requested_name.as_deref())
            .await?;
        let subagent_executor = if session_kind == "subagent" {
            let guard = self.subagent_executor.lock().await;
            guard.clone()
        } else {
            None
        };
        let session = request
            .create_session(
                tool_call_id,
                session_name,
                &self.terminal_executor,
                subagent_executor,
                context,
            )
            .await?;

        self.registry.save_aliases(tool_call_id, &session).await;
        let output = {
            let mut guard = session.lock().await;
            guard
                .start(args)
                .await
                .unwrap_or_else(|err| ToolOutput::error(name, format!("{err:#}")))
        };
        self.registry.touch(&session).await;
        Ok(output)
    }

    pub(crate) async fn operate(&self, spec: ToolSpec, args: &Map<String, Value>) -> Value {
        let session = self.registry.resolve(spec.kind, args).await;
        let Some(session) = session else {
            return session_not_found(spec, args);
        };
        match operate_on_session(&session, spec.name, args).await {
            Ok(value) => {
                if spec.name == "close_subagent" {
                    self.registry.remove_aliases_for(&session).await;
                } else {
                    self.registry.touch(&session).await;
                }
                value
            }
            Err(err) => ToolOutput::error(spec.name, format!("{err:#}")),
        }
    }

    pub(crate) async fn wait(&self, args: &Map<String, Value>) -> Value {
        let (seconds, target) = match parse_wait_target(args) {
            Ok(parsed) => parsed,
            Err(err) => return ToolOutput::error("wait", format!("{err:#}")),
        };
        let (kind, name) = match target {
            WaitTarget::Sleep => {
                return wait_seconds(seconds)
                    .await
                    .unwrap_or_else(|err| ToolOutput::error("wait", format!("{err:#}")));
            }
            WaitTarget::Terminal(name) => ("terminal", name),
            WaitTarget::Subagent(name) => ("subagent", name),
        };

        let Some(session) = self.registry.resolve_name(kind, &name).await else {
            return named_session_not_found("wait", kind, &name);
        };
        let output = wait_session(&session, seconds)
            .await
            .unwrap_or_else(|err| ToolOutput::error("wait", format!("{err:#}")));
        self.registry.touch(&session).await;
        output
    }

    pub(crate) async fn list(&self, spec: ToolSpec) -> Value {
        self.registry.list_sessions(spec.name, spec.kind).await
    }
}

async fn operate_on_session(
    session: &Arc<Mutex<dyn ToolSession>>,
    name: &str,
    args: &Map<String, Value>,
) -> Result<Value> {
    let mut guard = session.lock().await;
    match name {
        "terminal_checkout" => {
            if !guard.capabilities().contains(&Cap::Checkout) {
                return Ok(ToolOutput::error(name, "session does not support checkout"));
            }
            let mut op_args = Map::new();
            op_args.insert("tool".to_string(), Value::String(name.to_string()));
            if let Some(value) = args.get("lines").cloned() {
                op_args.insert("lines".to_string(), value);
            }
            guard.checkout(&op_args).await
        }
        "checkout_subagent" => {
            if !guard.capabilities().contains(&Cap::Checkout) {
                return Ok(ToolOutput::error(name, "session does not support checkout"));
            }
            let mut op_args = Map::new();
            op_args.insert("tool".to_string(), Value::String(name.to_string()));
            if let Some(value) = args.get("message_num").cloned() {
                op_args.insert("message_num".to_string(), value);
            }
            guard.checkout(&op_args).await
        }
        "terminal_kill" => {
            guard.cancel().await?;
            let mut op_args = Map::new();
            op_args.insert("tool".to_string(), Value::String(name.to_string()));
            if let Some(value) = args.get("lines").cloned() {
                op_args.insert("lines".to_string(), value);
            }
            guard.checkout(&op_args).await
        }
        "close_subagent" => {
            guard.cancel().await?;
            let item = guard.list_item();
            Ok(json!({
                "tool": "close_subagent",
                "kind": "subagent",
                "name": item.get("name").cloned().unwrap_or(Value::Null),
                "id": item.get("id").cloned().unwrap_or(Value::Null),
                "status": "ok",
            }))
        }
        "send_subagent" => {
            if !guard.capabilities().contains(&Cap::Send) {
                return Ok(ToolOutput::error(name, "session does not support send"));
            }
            let message = args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let interrupt = args
                .get("interrupt")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            guard.send(&message, interrupt).await
        }
        other => Ok(ToolOutput::error(other, format!("Unknown tool: {other}"))),
    }
}

struct SessionRegistry {
    state: Mutex<SessionState>,
    finished_ttl_seconds: u64,
}

struct SessionState {
    sessions: HashMap<String, Arc<Mutex<dyn ToolSession>>>,
    updated_at: HashMap<String, Instant>,
    reserved_session_keys: std::collections::HashSet<String>,
    terminal_counter: u64,
    subagent_counter: u64,
    closing: bool,
}

impl SessionRegistry {
    fn new(finished_ttl_seconds: u64) -> Self {
        Self {
            state: Mutex::new(SessionState {
                sessions: HashMap::new(),
                updated_at: HashMap::new(),
                reserved_session_keys: std::collections::HashSet::new(),
                terminal_counter: 0,
                subagent_counter: 0,
                closing: false,
            }),
            finished_ttl_seconds: finished_ttl_seconds.max(30),
        }
    }

    async fn is_closing(&self) -> bool {
        self.state.lock().await.closing
    }

    async fn take_all_for_shutdown(&self) -> Vec<Arc<Mutex<dyn ToolSession>>> {
        let mut state = self.state.lock().await;
        state.closing = true;
        let sessions = unique_sessions(state.sessions.values());
        state.sessions.clear();
        state.updated_at.clear();
        state.reserved_session_keys.clear();
        sessions
    }

    async fn unique_sessions_and_clear_reservations(&self) -> Vec<Arc<Mutex<dyn ToolSession>>> {
        let mut state = self.state.lock().await;
        let sessions = unique_sessions(state.sessions.values());
        state.reserved_session_keys.clear();
        sessions
    }

    async fn remove_aliases_for(&self, session: &Arc<Mutex<dyn ToolSession>>) {
        let mut state = self.state.lock().await;
        let keys: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, existing)| Arc::ptr_eq(existing, session))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            state.sessions.remove(&key);
            state.updated_at.remove(&key);
        }
    }

    async fn get(&self, key: &str) -> Option<Arc<Mutex<dyn ToolSession>>> {
        self.state.lock().await.sessions.get(key).cloned()
    }

    async fn resolve(
        &self,
        kind: &str,
        args: &Map<String, Value>,
    ) -> Option<Arc<Mutex<dyn ToolSession>>> {
        let name_key = match kind {
            "terminal" => "terminal_name",
            "subagent" => "subagent_name",
            _ => return None,
        };
        let name = args.get(name_key).and_then(Value::as_str)?;
        self.resolve_name(kind, name).await
    }

    async fn resolve_name(&self, kind: &str, name: &str) -> Option<Arc<Mutex<dyn ToolSession>>> {
        self.get(&session_key(kind, name)).await
    }

    async fn allocate_name(&self, kind: &str, requested: Option<&str>) -> Result<String> {
        let mut state = self.state.lock().await;
        if let Some(name) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            let key = session_key(kind, name);
            if state.sessions.contains_key(&key) || state.reserved_session_keys.contains(&key) {
                anyhow::bail!("{kind} name already exists: {name}");
            }
            state.reserved_session_keys.insert(key);
            return Ok(name.to_string());
        }

        loop {
            let candidate = match kind {
                "terminal" => {
                    state.terminal_counter += 1;
                    format!("{}-{}", default_terminal_prefix(), state.terminal_counter)
                }
                "subagent" => {
                    let index = state.subagent_counter as usize;
                    state.subagent_counter += 1;
                    if index < SUBAGENT_NAMES.len() {
                        SUBAGENT_NAMES[index].to_string()
                    } else {
                        format!("subagent-{}", index + 1)
                    }
                }
                _ => anyhow::bail!("unknown session kind: {kind}"),
            };
            let key = session_key(kind, &candidate);
            if !state.sessions.contains_key(&key) && !state.reserved_session_keys.contains(&key) {
                state.reserved_session_keys.insert(key);
                return Ok(candidate);
            }
        }
    }

    async fn save_aliases(&self, id: &str, session: &Arc<Mutex<dyn ToolSession>>) {
        let item = {
            let guard = session.lock().await;
            guard.list_item()
        };
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        self.save(id, session.clone()).await;
        if !kind.is_empty() && !name.is_empty() {
            self.save(&session_key(kind, name), session.clone()).await;
        }
    }

    async fn save(&self, key: &str, session: Arc<Mutex<dyn ToolSession>>) {
        let key = key.trim().to_string();
        if key.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state.updated_at.insert(key.clone(), Instant::now());
        state.reserved_session_keys.remove(&key);
        state.sessions.insert(key, session);
    }

    async fn touch(&self, session: &Arc<Mutex<dyn ToolSession>>) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let keys: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, existing)| Arc::ptr_eq(existing, session))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            state.updated_at.insert(key, now);
        }
    }

    async fn list_sessions(&self, tool: &str, target_kind: &str) -> Value {
        let sessions = {
            let state = self.state.lock().await;
            unique_sessions(state.sessions.values())
        };

        let mut items = Vec::new();
        for session in sessions {
            let guard = session.lock().await;
            let item = guard.list_item();
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == target_kind {
                items.push(item);
            }
        }
        ToolOutput::completed(tool, target_kind)
            .field("items", Value::Array(items))
            .into_value()
    }

    async fn prune_finished(&self) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let ttl = Duration::from_secs(self.finished_ttl_seconds);
        let snapshot: Vec<(String, Arc<Mutex<dyn ToolSession>>, Instant)> = state
            .sessions
            .iter()
            .map(|(key, session)| {
                (
                    key.clone(),
                    session.clone(),
                    state.updated_at.get(key).copied().unwrap_or(now),
                )
            })
            .collect();
        let expired: Vec<String> = snapshot
            .into_iter()
            .filter_map(|(key, session, updated)| {
                let expires_when_done = session
                    .try_lock()
                    .map(|guard| {
                        let is_done = guard.is_done();
                        let kind = guard
                            .list_item()
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        is_done && kind != "subagent"
                    })
                    .unwrap_or(false);
                (expires_when_done && now.saturating_duration_since(updated) >= ttl).then_some(key)
            })
            .collect();
        for key in expired {
            state.sessions.remove(&key);
            state.updated_at.remove(&key);
        }
    }
}

fn session_not_found(spec: ToolSpec, args: &Map<String, Value>) -> Value {
    match spec.kind {
        "terminal" => {
            let name = args
                .get("terminal_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if !name.is_empty() {
                return named_session_not_found(spec.name, spec.kind, name);
            }
        }
        "subagent" => {
            let name = args
                .get("subagent_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if !name.is_empty() {
                return named_session_not_found(spec.name, spec.kind, name);
            }
        }
        _ => {}
    }
    ToolOutput::error(spec.name, "session not found")
}

fn named_session_not_found(tool: &str, kind: &str, name: &str) -> Value {
    ToolOutput::new(tool, kind, "error")
        .field("name", Value::String(name.to_string()))
        .field("error", Value::String(format!("{kind} not found")))
        .into_value()
}

fn unique_sessions<'a>(
    sessions: impl IntoIterator<Item = &'a Arc<Mutex<dyn ToolSession>>>,
) -> Vec<Arc<Mutex<dyn ToolSession>>> {
    let mut unique = Vec::new();
    for session in sessions {
        if !unique.iter().any(|existing| Arc::ptr_eq(existing, session)) {
            unique.push(session.clone());
        }
    }
    unique
}

fn session_key(kind: &str, name: &str) -> String {
    format!("{kind}:{}", name.trim())
}

fn default_terminal_prefix() -> &'static str {
    if cfg!(windows) { "powershell" } else { "sh" }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::tools::tool_catalog::lookup_tool;

    struct FakeSession {
        id: String,
        name: String,
        kind: String,
        done: bool,
        cancel_count: Arc<AtomicUsize>,
    }

    impl FakeSession {
        fn new(kind: &str, name: &str, done: bool) -> (Self, Arc<AtomicUsize>) {
            let cancel_count = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    id: format!("{kind}-{name}"),
                    name: name.to_string(),
                    kind: kind.to_string(),
                    done,
                    cancel_count: cancel_count.clone(),
                },
                cancel_count,
            )
        }
    }

    #[async_trait]
    impl ToolSession for FakeSession {
        fn session_id(&self) -> &str {
            &self.id
        }

        async fn start(&mut self, _args: &Map<String, Value>) -> Result<Value> {
            Ok(json!({ "status": "ok" }))
        }

        async fn cancel(&mut self) -> Result<()> {
            self.cancel_count.fetch_add(1, Ordering::SeqCst);
            self.done = true;
            Ok(())
        }

        fn is_done(&self) -> bool {
            self.done
        }

        fn list_item(&self) -> Value {
            json!({
                "id": self.id,
                "name": self.name,
                "kind": self.kind,
                "status": if self.done { "completed" } else { "running" },
            })
        }
    }

    #[tokio::test]
    async fn prune_finished_removes_terminals_but_keeps_subagents() {
        let registry = SessionRegistry::new(30);
        let (terminal, _) = FakeSession::new("terminal", "powershell-1", true);
        let terminal = Arc::new(Mutex::new(terminal)) as Arc<Mutex<dyn ToolSession>>;
        let (subagent, _) = FakeSession::new("subagent", "alice", true);
        let subagent = Arc::new(Mutex::new(subagent)) as Arc<Mutex<dyn ToolSession>>;

        registry.save_aliases("terminal-call", &terminal).await;
        registry.save_aliases("subagent-call", &subagent).await;
        {
            let mut state = registry.state.lock().await;
            let old = Instant::now() - Duration::from_secs(31);
            for updated in state.updated_at.values_mut() {
                *updated = old;
            }
        }

        registry.prune_finished().await;

        assert!(
            registry
                .resolve_name("terminal", "powershell-1")
                .await
                .is_none()
        );
        assert!(registry.resolve_name("subagent", "alice").await.is_some());
    }

    #[tokio::test]
    async fn close_subagent_removes_registered_aliases() {
        let manager = SessionManager::new(std::env::current_dir().unwrap(), 30);
        let (subagent, cancel_count) = FakeSession::new("subagent", "alice", false);
        let subagent = Arc::new(Mutex::new(subagent)) as Arc<Mutex<dyn ToolSession>>;
        manager
            .registry
            .save_aliases("subagent-call", &subagent)
            .await;

        let output = manager
            .operate(
                lookup_tool("close_subagent").unwrap(),
                json!({ "subagent_name": "alice" }).as_object().unwrap(),
            )
            .await;

        assert_eq!(output["status"], "ok");
        assert_eq!(cancel_count.load(Ordering::SeqCst), 1);
        assert!(manager.registry.get("subagent-call").await.is_none());
        assert!(
            manager
                .registry
                .resolve_name("subagent", "alice")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancel_running_tools_leaves_subagents_alive() {
        let manager = SessionManager::new(std::env::current_dir().unwrap(), 30);
        let (terminal, terminal_cancel_count) = FakeSession::new("terminal", "powershell-1", false);
        let terminal = Arc::new(Mutex::new(terminal)) as Arc<Mutex<dyn ToolSession>>;
        let (subagent, subagent_cancel_count) = FakeSession::new("subagent", "alice", false);
        let subagent = Arc::new(Mutex::new(subagent)) as Arc<Mutex<dyn ToolSession>>;

        manager
            .registry
            .save_aliases("terminal-call", &terminal)
            .await;
        manager
            .registry
            .save_aliases("subagent-call", &subagent)
            .await;

        manager.cancel_running_tools().await;

        assert_eq!(terminal_cancel_count.load(Ordering::SeqCst), 1);
        assert_eq!(subagent_cancel_count.load(Ordering::SeqCst), 0);
        assert!(
            manager
                .registry
                .resolve_name("terminal", "powershell-1")
                .await
                .is_none()
        );
        assert!(
            manager
                .registry
                .resolve_name("subagent", "alice")
                .await
                .is_some()
        );
    }
}
