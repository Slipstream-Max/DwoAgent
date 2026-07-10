//! Machine-level supervisor configuration and lifecycle.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dwo_agent_core::agent::constants::{
    MODE_CONFIRM, MODE_FULL_ACCESS, MODE_WATCH, parse_policy_mode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioCommand};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use uuid::Uuid;

mod gateway;

const WINDOWS_TASK_NAME: &str = "DwoAgentSupervisor";
const WINDOWS_LAUNCHER_FILE: &str = "supervisor-startup.vbs";
const MACOS_LAUNCH_AGENT_ID: &str = "com.dwoagent.supervisor";
const LINUX_SYSTEMD_UNIT: &str = "dwoagent-supervisor.service";
const LINUX_DESKTOP_FILE: &str = "dwoagent-supervisor.desktop";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SupervisorConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub endpoint: SupervisorEndpointConfig,
    #[serde(default)]
    pub profiles: Vec<SupervisorProfileConfig>,
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub pool: SupervisorPoolConfig,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            endpoint: SupervisorEndpointConfig::default(),
            profiles: Vec::new(),
            default_profile: None,
            pool: SupervisorPoolConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SupervisorEndpointConfig {
    #[serde(default = "default_websocket_bind_addr")]
    pub websocket_bind_addr: String,
    #[serde(default = "generate_secret")]
    pub secret: String,
}

impl Default for SupervisorEndpointConfig {
    fn default() -> Self {
        Self {
            websocket_bind_addr: default_websocket_bind_addr(),
            secret: generate_secret(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorProfileConfig {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SupervisorPoolConfig {
    #[serde(default = "default_max_workers")]
    pub max_workers: usize,
    #[serde(default = "default_idle_seconds")]
    pub idle_seconds: u64,
}

impl Default for SupervisorPoolConfig {
    fn default() -> Self {
        Self {
            max_workers: default_max_workers(),
            idle_seconds: default_idle_seconds(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SupervisorProcess {
    pub pid: u32,
    pub command_line: String,
}

pub fn default_supervisor_config_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".dwoagent")
        .join("supervisor.yaml")
}

pub fn load_supervisor_config(path: Option<&Path>) -> Result<SupervisorConfig> {
    let path = path
        .map(PathBuf::from)
        .unwrap_or_else(default_supervisor_config_path);
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub fn run_supervisor_sync(config_path: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_supervisor(config_path))
}

pub fn run_acp_shim_sync(agent_profile: PathBuf) -> Result<()> {
    let (config, profile_id) = resolve_registered_profile(&agent_profile)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_acp_shim(config, profile_id))
}

fn resolve_registered_profile(agent_profile: &Path) -> Result<(SupervisorConfig, String)> {
    let config_path = default_supervisor_config_path();
    if !config_path.is_file() {
        bail!(
            "supervisor config not found at {}; run `dwoagent create supervisor --default` and register this profile",
            config_path.display()
        );
    }
    let config = load_supervisor_config(Some(&config_path))?;
    let requested = normalize_path(agent_profile);
    let Some(profile) = config
        .profiles
        .iter()
        .find(|profile| normalize_path(&profile.path) == requested)
    else {
        bail!(
            "agent profile {} is not registered in {}; add it to supervisor profiles",
            agent_profile.display(),
            config_path.display()
        );
    };
    Ok((config.clone(), profile.id.clone()))
}

async fn run_acp_shim(config: SupervisorConfig, profile_id: String) -> Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let stream = tokio::net::TcpStream::connect(&config.endpoint.websocket_bind_addr)
        .await
        .with_context(|| {
            format!(
                "connect supervisor websocket at {}",
                config.endpoint.websocket_bind_addr
            )
        })?;
    let url = format!("ws://{}", config.endpoint.websocket_bind_addr);
    let (websocket, _) = tokio_tungstenite::client_async(url, stream)
        .await
        .context("open supervisor websocket")?;
    let (mut write, mut read) = websocket.split();
    let (supervisor_tx, mut supervisor_rx) = mpsc::unbounded_channel::<Value>();

    let suppressed_ids = Arc::new(Mutex::new(HashSet::<String>::new()));
    let suppressed_for_stdin = suppressed_ids.clone();
    let secret = config.endpoint.secret.clone();
    let stdin_profile = profile_id.clone();
    let stdin_supervisor_tx = supervisor_tx.clone();
    let stdin_task = tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        while let Some(line) = stdin.next_line().await.context("read ACP stdin")? {
            let message: Value = serde_json::from_str(&line).context("parse ACP JSON-RPC")?;
            if message.get("method").is_none() {
                stdin_supervisor_tx
                    .send(json!({
                        "type": "client.response",
                        "secret": secret,
                        "message": message,
                    }))
                    .context("queue ACP client response")?;
                continue;
            }
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .context("ACP message requires method")?;
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let supervisor_message = match message.get("id").cloned() {
                Some(id) => json!({
                    "id": id,
                    "type": "worker.request",
                    "secret": secret,
                    "profile": stdin_profile,
                    "method": method,
                    "params": params,
                }),
                None => {
                    let id = format!("_dwo_acp_notify_{}", Uuid::new_v4());
                    suppressed_for_stdin.lock().await.insert(id.clone());
                    json!({
                        "id": id,
                        "type": "worker.notify",
                        "secret": secret,
                        "profile": stdin_profile,
                        "method": method,
                        "params": params,
                    })
                }
            };
            stdin_supervisor_tx
                .send(supervisor_message)
                .context("queue supervisor websocket message")?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let writer_task = tokio::spawn(async move {
        while let Some(message) = supervisor_rx.recv().await {
            write
                .send(Message::Text(message.to_string()))
                .await
                .context("write supervisor websocket message")?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let stdout_for_ws = stdout.clone();
    let ws_task = tokio::spawn(async move {
        while let Some(message) = read.next().await {
            let message = message.context("read supervisor websocket message")?;
            if !message.is_text() && !message.is_binary() {
                continue;
            }
            let text = message.into_text().context("read supervisor text")?;
            let value: Value = serde_json::from_str(&text).context("parse supervisor message")?;
            let Some(kind) = value.get("type").and_then(Value::as_str) else {
                continue;
            };
            match kind {
                "supervisor.ready" => {}
                "supervisor.event" => {
                    if let Some(event) = value.get("event") {
                        let out = json!({
                            "jsonrpc": "2.0",
                            "method": event.get("method").cloned().unwrap_or(Value::Null),
                            "params": event.get("params").cloned().unwrap_or(Value::Null),
                        });
                        write_stdout_json(&stdout_for_ws, out).await?;
                    }
                }
                "supervisor.client_request" => {
                    let out = json!({
                        "jsonrpc": "2.0",
                        "id": value.get("id").cloned().unwrap_or(Value::Null),
                        "method": value.get("method").cloned().unwrap_or(Value::Null),
                        "params": value.get("params").cloned().unwrap_or(Value::Null),
                    });
                    write_stdout_json(&stdout_for_ws, out).await?;
                }
                "supervisor.result" | "supervisor.error" => {
                    let id = value.get("id").cloned().unwrap_or(Value::Null);
                    if id.is_null() {
                        continue;
                    }
                    if let Some(id_text) = id.as_str()
                        && suppressed_ids.lock().await.remove(id_text)
                    {
                        continue;
                    }
                    let out = if kind == "supervisor.result" {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": value
                                .get("result")
                                .and_then(|result| result.get("result"))
                                .cloned()
                                .unwrap_or(Value::Null),
                        })
                    } else {
                        let error = value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("supervisor request failed");
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32603,
                                "message": error,
                                "data": error,
                            },
                        })
                    };
                    write_stdout_json(&stdout_for_ws, out).await?;
                    if kind == "supervisor.result"
                        && let Some(events) = value
                            .get("result")
                            .and_then(|result| result.get("replayEvents"))
                            .and_then(Value::as_array)
                    {
                        for event in events {
                            let out = json!({
                                "jsonrpc": "2.0",
                                "method": event.get("method").cloned().unwrap_or(Value::Null),
                                "params": event.get("params").cloned().unwrap_or(Value::Null),
                            });
                            write_stdout_json(&stdout_for_ws, out).await?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    tokio::select! {
        result = stdin_task => result.context("ACP stdin task panicked")?,
        result = ws_task => result.context("supervisor websocket task panicked")?,
        result = writer_task => result.context("supervisor websocket writer task panicked")?,
    }
}

async fn write_stdout_json(stdout: &Arc<Mutex<tokio::io::Stdout>>, value: Value) -> Result<()> {
    let mut line = serde_json::to_vec(&value).context("serialize ACP stdout")?;
    line.push(b'\n');
    let mut stdout = stdout.lock().await;
    stdout.write_all(&line).await.context("write ACP stdout")?;
    stdout.flush().await.context("flush ACP stdout")?;
    Ok(())
}

async fn run_supervisor(config_path: Option<PathBuf>) -> Result<()> {
    let config = load_supervisor_config(config_path.as_deref())?;
    let state = Arc::new(SupervisorState::new(config.clone())?);
    let gateway_tasks = gateway::spawn_gateways(state.clone()).await;
    let listener = tokio::net::TcpListener::bind(&config.endpoint.websocket_bind_addr)
        .await
        .with_context(|| {
            format!(
                "bind supervisor websocket at {}",
                config.endpoint.websocket_bind_addr
            )
        })?;
    let bind_addr = listener.local_addr().context("read supervisor bind addr")?;
    tracing::info!(%bind_addr, "dwoagent supervisor listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer_addr) = accepted.context("accept supervisor websocket tcp")?;
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_supervisor_connection(stream, state).await {
                        tracing::warn!(%peer_addr, error = %format!("{err:#}"), "supervisor connection failed");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                for task in gateway_tasks {
                    task.abort();
                }
                state.worker_pool.shutdown_all().await;
                return Ok(());
            },
        }
    }
}

struct SupervisorState {
    config: SupervisorConfig,
    worker_pool: WorkerPool,
    event_bus: Arc<SessionEventBus>,
    config_cache: Arc<SessionConfigCache>,
    permissions: Arc<SupervisorPermissionRegistry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    profile: String,
    session_id: String,
}

#[derive(Clone)]
struct SupervisorConnectionContext {
    id: String,
    sender: mpsc::UnboundedSender<Value>,
}

#[derive(Default)]
struct SessionEventBus {
    subscribers: Mutex<HashMap<SessionKey, HashMap<String, mpsc::UnboundedSender<Value>>>>,
    by_connection: Mutex<HashMap<String, HashSet<SessionKey>>>,
    prompt_origins: Mutex<HashMap<SessionKey, PromptOrigin>>,
}

#[derive(Clone)]
struct PromptOrigin {
    connection_id: String,
    token: String,
}

#[derive(Default)]
struct SessionConfigCache {
    sessions: Mutex<HashMap<SessionKey, Vec<Value>>>,
}

#[derive(Clone)]
struct PendingPermissionSnapshot {
    confirmation_id: String,
    request_id: Value,
    profile: String,
    session_id: String,
    params: Value,
}

#[derive(Default)]
struct SupervisorPermissionRegistry {
    pending: Mutex<HashMap<String, PendingPermissionSnapshot>>,
    by_request: Mutex<HashMap<String, String>>,
    stale_requests: Mutex<HashSet<String>>,
    stale_confirmations: Mutex<HashSet<String>>,
}

impl SupervisorPermissionRegistry {
    fn new() -> Self {
        Self::default()
    }

    async fn register_client_request(
        &self,
        profile: &str,
        event: &Value,
    ) -> Option<PendingPermissionSnapshot> {
        if event.get("type").and_then(Value::as_str) != Some("client_request")
            || event.get("method").and_then(Value::as_str) != Some("session/request_permission")
        {
            return None;
        }
        let request_id = event.get("id")?.clone();
        let request_key = worker_response_key(&request_id);
        let params = event.get("params")?.clone();
        let session_id = params.get("sessionId").and_then(Value::as_str)?.to_string();
        let confirmation_id = short_confirmation_id();
        let snapshot = PendingPermissionSnapshot {
            confirmation_id: confirmation_id.clone(),
            request_id,
            profile: profile.to_string(),
            session_id,
            params,
        };
        self.pending
            .lock()
            .await
            .insert(confirmation_id.clone(), snapshot.clone());
        self.by_request
            .lock()
            .await
            .insert(request_key.clone(), confirmation_id.clone());
        self.stale_requests.lock().await.remove(&request_key);
        self.stale_confirmations
            .lock()
            .await
            .remove(&confirmation_id);
        Some(snapshot)
    }

    async fn snapshot_by_request_id(
        &self,
        request_id: &Value,
    ) -> Option<PendingPermissionSnapshot> {
        let request_key = worker_response_key(request_id);
        let confirmation_id = self.by_request.lock().await.get(&request_key).cloned()?;
        self.pending.lock().await.get(&confirmation_id).cloned()
    }

    async fn take_confirmation(&self, confirmation_id: &str) -> Option<PendingPermissionSnapshot> {
        let snapshot = self.pending.lock().await.remove(confirmation_id)?;
        let request_key = worker_response_key(&snapshot.request_id);
        self.by_request.lock().await.remove(&request_key);
        self.stale_requests.lock().await.insert(request_key);
        self.stale_confirmations
            .lock()
            .await
            .insert(confirmation_id.to_string());
        Some(snapshot)
    }

    async fn is_stale_confirmation(&self, confirmation_id: &str) -> bool {
        self.stale_confirmations
            .lock()
            .await
            .contains(confirmation_id)
    }

    async fn should_forward_client_response(&self, message: &Value) -> bool {
        let Some(request_id) = message.get("id") else {
            return true;
        };
        let request_key = worker_response_key(request_id);
        if self.stale_requests.lock().await.contains(&request_key) {
            return false;
        }
        let confirmation_id = self.by_request.lock().await.remove(&request_key);
        if let Some(confirmation_id) = confirmation_id {
            self.pending.lock().await.remove(&confirmation_id);
            self.stale_requests.lock().await.insert(request_key);
            self.stale_confirmations
                .lock()
                .await
                .insert(confirmation_id);
        }
        true
    }

    async fn invalidate_session(&self, profile: &str, session_id: &str) {
        let stale = {
            let mut pending = self.pending.lock().await;
            let stale_ids = pending
                .iter()
                .filter(|(_, item)| item.profile == profile && item.session_id == session_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            stale_ids
                .into_iter()
                .filter_map(|id| pending.remove(&id).map(|item| (id, item)))
                .collect::<Vec<_>>()
        };
        if stale.is_empty() {
            return;
        }
        let mut by_request = self.by_request.lock().await;
        let mut stale_requests = self.stale_requests.lock().await;
        let mut stale_confirmations = self.stale_confirmations.lock().await;
        for (confirmation_id, item) in stale {
            let request_key = worker_response_key(&item.request_id);
            by_request.remove(&request_key);
            stale_requests.insert(request_key);
            stale_confirmations.insert(confirmation_id);
        }
    }
}

fn short_confirmation_id() -> String {
    Uuid::new_v4()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>()
}

impl SessionConfigCache {
    fn new() -> Self {
        Self::default()
    }

    async fn update_from_response(
        &self,
        profile: &str,
        session_id: Option<String>,
        response: &Value,
    ) {
        let Some(session_id) = session_id.or_else(|| session_id_from_response(response)) else {
            return;
        };
        let Some(config_options) = config_options_from_value(response) else {
            return;
        };
        self.sessions.lock().await.insert(
            SessionKey {
                profile: profile.to_string(),
                session_id,
            },
            config_options,
        );
    }

    async fn update_from_event(&self, profile: &str, event: &Value) {
        if event.get("method").and_then(Value::as_str) != Some("session/update") {
            return;
        }
        let Some(params) = event.get("params") else {
            return;
        };
        let Some(session_id) = session_id_from_params(params) else {
            return;
        };
        let Some(update) = params.get("update") else {
            return;
        };

        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("config_option_update") => {
                if let Some(config_options) = config_options_from_value(update) {
                    self.merge_config_options(profile, &session_id, config_options)
                        .await;
                }
            }
            Some("current_mode_update") => {
                if let Some(value) = update.get("currentModeId").and_then(Value::as_str) {
                    self.apply_config_value(profile, &session_id, "policy_mode", value)
                        .await;
                }
            }
            _ => {}
        }
    }

    async fn apply_control(&self, profile: &str, control: &ControlRequest) -> Vec<Value> {
        let key = SessionKey {
            profile: profile.to_string(),
            session_id: control.session_id.clone(),
        };
        let mut sessions = self.sessions.lock().await;
        let options = sessions
            .entry(key)
            .or_insert_with(|| vec![fallback_config_option("policy_mode", MODE_FULL_ACCESS)]);
        let Some(index) = options.iter().position(|option| {
            option.get("id").and_then(Value::as_str) == Some(&control.config_id)
        }) else {
            let option = fallback_config_option(&control.config_id, &control.value);
            options.push(option);
            return options.clone();
        };
        if let Some(option) = options[index].as_object_mut() {
            option.insert(
                "currentValue".to_string(),
                Value::String(control.value.clone()),
            );
        }
        ensure_current_value_is_selectable(&mut options[index]);
        options.clone()
    }

    async fn apply_config_value(
        &self,
        profile: &str,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) {
        let control = ControlRequest {
            session_id: session_id.to_string(),
            config_id: config_id.to_string(),
            value: value.to_string(),
            response_kind: ControlResponseKind::SetConfigOption,
        };
        self.apply_control(profile, &control).await;
    }

    async fn merge_config_options(&self, profile: &str, session_id: &str, incoming: Vec<Value>) {
        let key = SessionKey {
            profile: profile.to_string(),
            session_id: session_id.to_string(),
        };
        let mut sessions = self.sessions.lock().await;
        let cached = sessions.entry(key).or_default();
        for option in incoming {
            let Some(option_id) = option.get("id").and_then(Value::as_str) else {
                continue;
            };
            match cached
                .iter()
                .position(|item| item.get("id").and_then(Value::as_str) == Some(option_id))
            {
                Some(index) => cached[index] = option,
                None => cached.push(option),
            }
        }
    }
}

impl SessionEventBus {
    fn new() -> Self {
        Self::default()
    }

    async fn subscribe(
        &self,
        connection: &SupervisorConnectionContext,
        profile: &str,
        session_id: &str,
    ) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let key = SessionKey {
            profile: profile.to_string(),
            session_id: session_id.to_string(),
        };
        {
            let mut by_connection = self.by_connection.lock().await;
            by_connection
                .entry(connection.id.clone())
                .or_default()
                .insert(key.clone());
        }
        let mut subscribers = self.subscribers.lock().await;
        subscribers
            .entry(key)
            .or_default()
            .insert(connection.id.clone(), connection.sender.clone());
    }

    async fn unregister_connection(&self, connection_id: &str) {
        let keys = {
            let mut by_connection = self.by_connection.lock().await;
            by_connection.remove(connection_id).unwrap_or_default()
        };
        {
            let mut prompt_origins = self.prompt_origins.lock().await;
            prompt_origins.retain(|_, origin| origin.connection_id != connection_id);
        }
        if keys.is_empty() {
            return;
        }
        let mut subscribers = self.subscribers.lock().await;
        for key in keys {
            let remove_key = match subscribers.get_mut(&key) {
                Some(items) => {
                    items.remove(connection_id);
                    items.is_empty()
                }
                None => false,
            };
            if remove_key {
                subscribers.remove(&key);
            }
        }
    }

    async fn broadcast_worker_event(
        &self,
        profile: &str,
        event: &Value,
        exclude_connection_id: Option<&str>,
    ) {
        let Some(session_id) = session_id_from_worker_event(event) else {
            return;
        };
        let key = SessionKey {
            profile: profile.to_string(),
            session_id,
        };
        let message = json!({
            "type": "supervisor.event",
            "profile": profile,
            "event": event,
        });
        let mut closed = Vec::new();
        {
            let mut subscribers = self.subscribers.lock().await;
            let Some(items) = subscribers.get_mut(&key) else {
                return;
            };
            for (connection_id, sender) in items.iter() {
                if exclude_connection_id == Some(connection_id.as_str()) {
                    continue;
                }
                if sender.send(message.clone()).is_err() {
                    closed.push(connection_id.clone());
                }
            }
            for connection_id in &closed {
                items.remove(connection_id);
            }
            if items.is_empty() {
                subscribers.remove(&key);
            }
        }
        if !closed.is_empty() {
            let mut by_connection = self.by_connection.lock().await;
            for connection_id in closed {
                if let Some(keys) = by_connection.get_mut(&connection_id) {
                    keys.remove(&key);
                    if keys.is_empty() {
                        by_connection.remove(&connection_id);
                    }
                }
            }
        }
    }

    async fn broadcast_worker_message(&self, profile: &str, event: &Value) {
        let (session_id, message) =
            if event.get("type").and_then(Value::as_str) == Some("client_request") {
                let Some(session_id) = event.get("params").and_then(session_id_from_params) else {
                    return;
                };
                (
                    session_id,
                    json!({
                        "type": "supervisor.client_request",
                        "profile": profile,
                        "id": event.get("id").cloned().unwrap_or(Value::Null),
                        "method": event.get("method").cloned().unwrap_or(Value::Null),
                        "params": event.get("params").cloned().unwrap_or(Value::Null),
                    }),
                )
            } else {
                let Some(session_id) = session_id_from_worker_event(event) else {
                    return;
                };
                (
                    session_id,
                    json!({
                        "type": "supervisor.event",
                        "profile": profile,
                        "event": event,
                    }),
                )
            };
        let key = SessionKey {
            profile: profile.to_string(),
            session_id,
        };
        let exclude_connection = if is_user_message_update_event(event) {
            self.prompt_origin_connection(&key).await
        } else {
            None
        };
        self.broadcast_to_key(&key, message, exclude_connection.as_deref())
            .await;
    }

    async fn begin_prompt_origin(
        &self,
        connection: &SupervisorConnectionContext,
        profile: &str,
        session_id: &str,
    ) -> Option<String> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return None;
        }
        let key = SessionKey {
            profile: profile.to_string(),
            session_id: session_id.to_string(),
        };
        let token = Uuid::new_v4().to_string();
        self.prompt_origins.lock().await.insert(
            key,
            PromptOrigin {
                connection_id: connection.id.clone(),
                token: token.clone(),
            },
        );
        Some(token)
    }

    async fn end_prompt_origin(&self, profile: &str, session_id: &str, token: &str) {
        let key = SessionKey {
            profile: profile.to_string(),
            session_id: session_id.to_string(),
        };
        let mut prompt_origins = self.prompt_origins.lock().await;
        if prompt_origins
            .get(&key)
            .is_some_and(|origin| origin.token == token)
        {
            prompt_origins.remove(&key);
        }
    }

    async fn prompt_origin_connection(&self, key: &SessionKey) -> Option<String> {
        self.prompt_origins
            .lock()
            .await
            .get(key)
            .map(|origin| origin.connection_id.clone())
    }

    async fn broadcast_to_key(
        &self,
        key: &SessionKey,
        message: Value,
        exclude_connection_id: Option<&str>,
    ) {
        let mut closed = Vec::new();
        {
            let mut subscribers = self.subscribers.lock().await;
            let Some(items) = subscribers.get_mut(key) else {
                return;
            };
            for (connection_id, sender) in items.iter() {
                if exclude_connection_id == Some(connection_id.as_str()) {
                    continue;
                }
                if sender.send(message.clone()).is_err() {
                    closed.push(connection_id.clone());
                }
            }
            for connection_id in &closed {
                items.remove(connection_id);
            }
            if items.is_empty() {
                subscribers.remove(key);
            }
        }
        if !closed.is_empty() {
            let mut by_connection = self.by_connection.lock().await;
            for connection_id in closed {
                if let Some(keys) = by_connection.get_mut(&connection_id) {
                    keys.remove(key);
                    if keys.is_empty() {
                        by_connection.remove(&connection_id);
                    }
                }
            }
        }
    }
}

impl SupervisorState {
    fn new(config: SupervisorConfig) -> Result<Self> {
        let event_bus = Arc::new(SessionEventBus::new());
        let config_cache = Arc::new(SessionConfigCache::new());
        let permissions = Arc::new(SupervisorPermissionRegistry::new());
        Ok(Self {
            worker_pool: WorkerPool::new(
                env::current_exe().context("resolve current executable")?,
                event_bus.clone(),
                config_cache.clone(),
                permissions.clone(),
            ),
            event_bus,
            config_cache,
            permissions,
            config,
        })
    }

    fn resolve_profile(&self, requested: Option<&str>) -> Result<ResolvedProfile> {
        let profile_id = match requested {
            Some(value) if !value.trim().is_empty() => value.trim(),
            _ => self
                .config
                .default_profile
                .as_deref()
                .context("no profile requested and supervisor.defaultProfile is not set")?,
        };
        let profile = self
            .config
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .with_context(|| format!("unknown supervisor profile `{profile_id}`"))?;
        Ok(ResolvedProfile {
            id: profile.id.clone(),
            path: profile.path.clone(),
        })
    }

    async fn handle_request_with_events<F, Fut>(
        &self,
        request: SupervisorRequest,
        connection: Option<SupervisorConnectionContext>,
        emit_event: F,
    ) -> SupervisorResponse
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let id = request.id.clone();
        match self
            .dispatch_with_events(request, connection, emit_event)
            .await
        {
            Ok(result) => SupervisorResponse::result(id, result),
            Err(err) => SupervisorResponse::error(id, err.to_string()),
        }
    }

    async fn dispatch_with_events<F, Fut>(
        &self,
        request: SupervisorRequest,
        connection: Option<SupervisorConnectionContext>,
        _emit_event: F,
    ) -> Result<Value>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.validate_secret(request.secret.as_deref())?;
        match request.kind.as_str() {
            "supervisor.ping" => Ok(json!({ "ok": true })),
            "client.response" => {
                let message = request
                    .message
                    .context("client.response requires `message`")?;
                if !self
                    .permissions
                    .should_forward_client_response(&message)
                    .await
                {
                    return Ok(json!({ "forwarded": false, "stale": true }));
                }
                self.forward_client_response_message(message).await?;
                Ok(json!({ "forwarded": true }))
            }
            "profiles.list" => Ok(json!({
                "profiles": self.config.profiles.iter().map(|profile| {
                    json!({
                        "id": profile.id,
                        "path": profile.path,
                        "default": self.config.default_profile.as_deref() == Some(profile.id.as_str()),
                    })
                }).collect::<Vec<_>>()
            })),
            "worker.request" => {
                let method = request
                    .method
                    .as_deref()
                    .context("worker.request requires `method`")?;
                let profile = self.resolve_profile(request.profile.as_deref())?;
                let params = request.params.unwrap_or(Value::Null);
                if let Some(result) = self
                    .handle_control_request(&profile, method, &params, connection.as_ref())
                    .await?
                {
                    return Ok(result);
                }
                let request_session_id = session_id_from_params(&params);
                if method == "session/cancel"
                    && let Some(session_id) = request_session_id.as_deref()
                {
                    self.permissions
                        .invalidate_session(&profile.id, session_id)
                        .await;
                }
                if let Some(connection) = connection.as_ref()
                    && should_subscribe_for_request(method)
                    && let Some(session_id) = request_session_id.as_deref()
                {
                    self.event_bus
                        .subscribe(connection, &profile.id, session_id)
                        .await;
                }
                let (worker_method, worker_params) = if method == "session/load" {
                    let session_id = request_session_id
                        .as_deref()
                        .context("session/load requires `sessionId`")?;
                    ("_dwo/session/load", json!({ "sessionId": session_id }))
                } else {
                    (method, params)
                };
                let prompt_origin = if method == "session/prompt" {
                    match (connection.as_ref(), request_session_id.as_deref()) {
                        (Some(connection), Some(session_id)) => self
                            .event_bus
                            .begin_prompt_origin(connection, &profile.id, session_id)
                            .await
                            .map(|token| (session_id.to_string(), token)),
                        _ => None,
                    }
                } else {
                    None
                };
                let worker_result = self
                    .worker_pool
                    .request(&profile, worker_method, worker_params, &self.config.pool)
                    .await;
                if let Some((session_id, token)) = prompt_origin {
                    let event_bus = self.event_bus.clone();
                    let profile_id = profile.id.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        event_bus
                            .end_prompt_origin(&profile_id, &session_id, &token)
                            .await;
                    });
                }
                let result = worker_result?;
                let (result, replay_events) = if method == "session/load" {
                    (
                        result.get("response").cloned().unwrap_or(Value::Null),
                        result.get("replayEvents").cloned(),
                    )
                } else {
                    (result, None)
                };
                self.config_cache
                    .update_from_response(&profile.id, request_session_id, &result)
                    .await;
                if let Some(connection) = connection.as_ref()
                    && method == "session/new"
                    && let Some(session_id) = session_id_from_response(&result)
                {
                    self.event_bus
                        .subscribe(connection, &profile.id, &session_id)
                        .await;
                }
                let mut response = json!({
                    "profile": profile.id,
                    "result": result,
                });
                if let Some(replay_events) = replay_events
                    && let Some(object) = response.as_object_mut()
                {
                    object.insert("replayEvents".to_string(), replay_events);
                }
                Ok(response)
            }
            "worker.notify" => {
                let method = request
                    .method
                    .as_deref()
                    .context("worker.notify requires `method`")?;
                let profile = self.resolve_profile(request.profile.as_deref())?;
                let params = request.params.unwrap_or(Value::Null);
                if method == "session/cancel"
                    && let Some(session_id) = session_id_from_params(&params)
                {
                    self.permissions
                        .invalidate_session(&profile.id, &session_id)
                        .await;
                }
                if let Some(connection) = connection.as_ref()
                    && should_subscribe_for_request(method)
                    && let Some(session_id) = session_id_from_params(&params)
                {
                    self.event_bus
                        .subscribe(connection, &profile.id, &session_id)
                        .await;
                }
                self.worker_pool
                    .notify(&profile, method, params, &self.config.pool)
                    .await?;
                Ok(json!({
                    "profile": profile.id,
                    "notified": true,
                }))
            }
            "worker.stop" => {
                let profile = self.resolve_profile(request.profile.as_deref())?;
                let stopped = self.worker_pool.stop_profile(&profile.id).await?;
                Ok(json!({
                    "profile": profile.id,
                    "stopped": stopped,
                }))
            }
            "workers.status" => Ok(self.worker_pool.status().await),
            "workers.shutdown" => {
                let stopped = self.worker_pool.shutdown_all().await;
                Ok(json!({ "stopped": stopped }))
            }
            other => bail!("unknown supervisor request type `{other}`"),
        }
    }

    async fn handle_control_request(
        &self,
        profile: &ResolvedProfile,
        method: &str,
        params: &Value,
        connection: Option<&SupervisorConnectionContext>,
    ) -> Result<Option<Value>> {
        let Some(control) = ControlRequest::from_worker_request(method, params)? else {
            return Ok(None);
        };
        if !self.worker_pool.is_profile_busy(&profile.id).await {
            return Ok(None);
        }
        let config_options = self.config_cache.apply_control(&profile.id, &control).await;
        self.worker_pool
            .notify(
                profile,
                "_dwo/session/set_config_option",
                json!({
                    "sessionId": control.session_id.clone(),
                    "configId": control.config_id.clone(),
                    "value": control.value.clone(),
                }),
                &self.config.pool,
            )
            .await?;
        if let Some(connection) = connection {
            self.event_bus
                .subscribe(connection, &profile.id, &control.session_id)
                .await;
        }
        self.event_bus
            .broadcast_worker_event(
                &profile.id,
                &control.session_update_event(config_options.clone()),
                None,
            )
            .await;
        Ok(Some(json!({
            "profile": profile.id,
            "result": control.response(config_options),
        })))
    }

    async fn worker_for_client_response(
        &self,
        response: &Value,
    ) -> Result<(Arc<WorkerProcess>, Value)> {
        let request_id = response
            .get("id")
            .context("client response requires `id`")?;
        let Some((profile_id, worker_request_id)) = pending_client_request_parts(request_id) else {
            bail!("client response id is not a supervisor-routed worker request");
        };
        let worker = self
            .worker_pool
            .get_worker(&profile_id)
            .await
            .with_context(|| format!("worker not found for profile `{profile_id}`"))?;
        Ok((worker, worker_request_id))
    }

    async fn forward_client_response_message(&self, mut message: Value) -> Result<()> {
        let (worker, worker_request_id) = self.worker_for_client_response(&message).await?;
        if let Some(object) = message.as_object_mut() {
            object.insert("id".to_string(), worker_request_id);
        }
        worker.forward_client_response(message).await
    }

    fn validate_secret(&self, secret: Option<&str>) -> Result<()> {
        if self.config.endpoint.secret.is_empty() {
            return Ok(());
        }
        match secret {
            Some(secret) if secret == self.config.endpoint.secret => Ok(()),
            _ => bail!("invalid supervisor secret"),
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedProfile {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct SupervisorRequest {
    id: Option<Value>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    message: Option<Value>,
}

struct ControlRequest {
    session_id: String,
    config_id: String,
    value: String,
    response_kind: ControlResponseKind,
}

enum ControlResponseKind {
    SetMode,
    SetConfigOption,
}

impl ControlRequest {
    fn from_worker_request(method: &str, params: &Value) -> Result<Option<Self>> {
        match method {
            "session/set_mode" => {
                let session_id = session_id_from_params(params)
                    .context("session/set_mode requires `sessionId`")?;
                let mode_id = params
                    .get("modeId")
                    .and_then(Value::as_str)
                    .context("session/set_mode requires `modeId`")?;
                let value = parse_policy_mode(mode_id)?;
                Ok(Some(Self {
                    session_id,
                    config_id: "policy_mode".to_string(),
                    value,
                    response_kind: ControlResponseKind::SetMode,
                }))
            }
            "session/set_config_option" => {
                let config_id = params
                    .get("configId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !matches!(config_id, "policy_mode" | "model" | "reasoning_mode") {
                    return Ok(None);
                }
                let session_id = session_id_from_params(params)
                    .context("session/set_config_option requires `sessionId`")?;
                let value = config_value_as_string(params.get("value"))
                    .context("session/set_config_option requires string `value`")?;
                let value = if config_id == "policy_mode" {
                    parse_policy_mode(&value)?
                } else {
                    value
                };
                Ok(Some(Self {
                    session_id,
                    config_id: config_id.to_string(),
                    value,
                    response_kind: ControlResponseKind::SetConfigOption,
                }))
            }
            _ => Ok(None),
        }
    }

    fn response(&self, config_options: Vec<Value>) -> Value {
        match self.response_kind {
            ControlResponseKind::SetMode => json!({}),
            ControlResponseKind::SetConfigOption => json!({
                "configOptions": config_options,
            }),
        }
    }

    fn session_update_event(&self, config_options: Vec<Value>) -> Value {
        if self.config_id == "policy_mode" {
            return json!({
                "method": "session/update",
                "params": {
                    "sessionId": self.session_id,
                    "update": {
                        "sessionUpdate": "current_mode_update",
                        "currentModeId": self.value,
                    },
                },
            });
        }
        json!({
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": config_options,
                },
            },
        })
    }
}

fn config_options_from_value(value: &Value) -> Option<Vec<Value>> {
    value
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|items| items.to_vec())
}

fn fallback_config_option(config_id: &str, value: &str) -> Value {
    let (name, category) = match config_id {
        "policy_mode" => ("Policy", "mode"),
        "model" => ("Model", "model"),
        "reasoning_mode" => ("Reasoning Mode", "thought_level"),
        other => (other, "mode"),
    };
    let options = if config_id == "policy_mode" {
        vec![
            json!({"value": MODE_FULL_ACCESS, "name": "Full Access"}),
            json!({"value": MODE_CONFIRM, "name": "Confirm"}),
            json!({"value": MODE_WATCH, "name": "Watch"}),
        ]
    } else if value.is_empty() {
        Vec::new()
    } else {
        vec![json!({"value": value, "name": value})]
    };
    json!({
        "id": config_id,
        "name": name,
        "category": category,
        "type": "select",
        "currentValue": value,
        "options": options,
    })
}

fn ensure_current_value_is_selectable(option: &mut Value) {
    let current_value = option
        .get("currentValue")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if current_value.is_empty() {
        return;
    }
    let Some(options) = option.get_mut("options").and_then(Value::as_array_mut) else {
        return;
    };
    let has_value = options
        .iter()
        .any(|item| item.get("value").and_then(Value::as_str) == Some(current_value.as_str()));
    if !has_value {
        options.push(json!({
            "value": current_value,
            "name": current_value,
        }));
    }
}

fn session_id_from_params(params: &Value) -> Option<String> {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn session_id_from_response(response: &Value) -> Option<String> {
    response
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn session_id_from_worker_event(event: &Value) -> Option<String> {
    if event.get("method").and_then(Value::as_str) != Some("session/update") {
        return None;
    }
    event.get("params").and_then(session_id_from_params)
}

fn is_user_message_update_event(event: &Value) -> bool {
    if event.get("method").and_then(Value::as_str) != Some("session/update") {
        return false;
    }
    event
        .get("params")
        .and_then(|params| params.get("update"))
        .and_then(|update| update.get("sessionUpdate"))
        .and_then(Value::as_str)
        == Some("user_message_chunk")
}

fn config_value_as_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.get("value").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

fn should_subscribe_for_request(method: &str) -> bool {
    matches!(
        method,
        "session/new"
            | "session/load"
            | "session/prompt"
            | "session/cancel"
            | "session/set_mode"
            | "session/set_config_option"
    )
}

#[derive(Debug, Serialize)]
struct SupervisorResponse {
    id: Option<Value>,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SupervisorResponse {
    fn result(id: Option<Value>, result: Value) -> Self {
        Self {
            id,
            kind: "supervisor.result",
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, error: String) -> Self {
        Self {
            id,
            kind: "supervisor.error",
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Serialize)]
struct WorkerRequest {
    jsonrpc: &'static str,
    id: String,
    method: String,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct WorkerMessage {
    id: Option<Value>,
    result: Option<Value>,
    error: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
}

struct WorkerPool {
    launcher: WorkerLauncher,
    workers: Mutex<HashMap<String, Arc<WorkerProcess>>>,
    event_bus: Arc<SessionEventBus>,
    config_cache: Arc<SessionConfigCache>,
    permissions: Arc<SupervisorPermissionRegistry>,
}

impl WorkerPool {
    fn new(
        exe: PathBuf,
        event_bus: Arc<SessionEventBus>,
        config_cache: Arc<SessionConfigCache>,
        permissions: Arc<SupervisorPermissionRegistry>,
    ) -> Self {
        Self {
            launcher: WorkerLauncher::agent_profile(exe),
            workers: Mutex::new(HashMap::new()),
            event_bus,
            config_cache,
            permissions,
        }
    }

    #[cfg(test)]
    fn new_with_launcher(
        launcher: WorkerLauncher,
        event_bus: Arc<SessionEventBus>,
        config_cache: Arc<SessionConfigCache>,
        permissions: Arc<SupervisorPermissionRegistry>,
    ) -> Self {
        Self {
            launcher,
            workers: Mutex::new(HashMap::new()),
            event_bus,
            config_cache,
            permissions,
        }
    }

    async fn request(
        &self,
        profile: &ResolvedProfile,
        method: &str,
        params: Value,
        pool_config: &SupervisorPoolConfig,
    ) -> Result<Value> {
        let worker = self.ensure_worker(profile, pool_config).await?;
        worker.touch().await;
        worker.request(method, params).await
    }

    async fn request_with_events<F, Fut>(
        &self,
        profile: &ResolvedProfile,
        method: &str,
        params: Value,
        pool_config: &SupervisorPoolConfig,
        mut emit_event: F,
    ) -> Result<Value>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let worker = self.ensure_worker(profile, pool_config).await?;
        worker.touch().await;
        let mut events = worker.subscribe_events();
        let request = worker.request(method, params);
        tokio::pin!(request);

        loop {
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(event) => emit_event(event).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return request.await,
                },
                result = &mut request => return result,
            }
        }
    }

    async fn notify(
        &self,
        profile: &ResolvedProfile,
        method: &str,
        params: Value,
        pool_config: &SupervisorPoolConfig,
    ) -> Result<()> {
        let worker = self.ensure_worker(profile, pool_config).await?;
        worker.touch().await;
        worker.notify(method, params).await
    }

    async fn is_profile_busy(&self, profile_id: &str) -> bool {
        let worker = {
            let workers = self.workers.lock().await;
            workers.get(profile_id).cloned()
        };
        match worker {
            Some(worker) => worker.is_busy().await,
            None => false,
        }
    }

    async fn get_worker(&self, profile_id: &str) -> Option<Arc<WorkerProcess>> {
        self.workers.lock().await.get(profile_id).cloned()
    }

    async fn ensure_worker(
        &self,
        profile: &ResolvedProfile,
        pool_config: &SupervisorPoolConfig,
    ) -> Result<Arc<WorkerProcess>> {
        self.prune_idle(pool_config).await;
        let mut workers = self.workers.lock().await;
        if let Some(worker) = workers.get(&profile.id) {
            return Ok(worker.clone());
        }

        while workers.len() >= pool_config.max_workers.max(1) {
            let Some(victim) = oldest_worker_key(&workers, Some(&profile.id)).await else {
                break;
            };
            if let Some(worker) = workers.remove(&victim) {
                tokio::spawn(async move {
                    let _ = stop_worker(worker).await;
                });
            }
        }

        let worker = Arc::new(
            WorkerProcess::spawn(&self.launcher, profile)
                .await
                .with_context(|| format!("spawn worker for profile `{}`", profile.id))?,
        );
        self.spawn_event_forwarder(profile.id.clone(), worker.clone());
        workers.insert(profile.id.clone(), worker.clone());
        Ok(worker)
    }

    fn spawn_event_forwarder(&self, profile_id: String, worker: Arc<WorkerProcess>) {
        let mut events = worker.subscribe_events();
        let event_bus = self.event_bus.clone();
        let config_cache = self.config_cache.clone();
        let permissions = self.permissions.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if event.get("type").and_then(Value::as_str) == Some("client_request") {
                            permissions
                                .register_client_request(&profile_id, &event)
                                .await;
                        } else {
                            config_cache.update_from_event(&profile_id, &event).await;
                            if is_user_message_update_event(&event)
                                && let Some(session_id) = session_id_from_worker_event(&event)
                            {
                                permissions
                                    .invalidate_session(&profile_id, &session_id)
                                    .await;
                            }
                        }
                        event_bus
                            .broadcast_worker_message(&profile_id, &event)
                            .await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    async fn prune_idle(&self, pool_config: &SupervisorPoolConfig) {
        let idle_after = Duration::from_secs(pool_config.idle_seconds.max(1));
        let now = Instant::now();
        let mut workers = self.workers.lock().await;
        let stale = {
            let mut stale = Vec::new();
            for (id, worker) in workers.iter() {
                if !worker.is_busy().await
                    && now.duration_since(worker.last_used().await) >= idle_after
                {
                    stale.push(id.clone());
                }
            }
            stale
        };
        for id in stale {
            if let Some(worker) = workers.remove(&id) {
                tokio::spawn(async move {
                    let _ = stop_worker(worker).await;
                });
            }
        }
    }

    async fn status(&self) -> Value {
        let workers = self.workers.lock().await;
        let mut items = Vec::new();
        for (profile, worker) in workers.iter() {
            items.push(json!({
                "profile": profile,
                "pid": worker.child_id().await,
            }));
        }
        json!({ "workers": items })
    }

    async fn stop_profile(&self, profile_id: &str) -> Result<bool> {
        let worker = {
            let mut workers = self.workers.lock().await;
            workers.remove(profile_id)
        };
        match worker {
            Some(worker) => {
                stop_worker(worker).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn shutdown_all(&self) -> usize {
        let workers = {
            let mut workers = self.workers.lock().await;
            std::mem::take(&mut *workers)
        };
        let stopped = workers.len();
        for (_, worker) in workers {
            let _ = stop_worker(worker).await;
        }
        stopped
    }
}

enum WorkerLauncher {
    AgentProfile {
        exe: PathBuf,
    },
    #[cfg(test)]
    Custom {
        exe: PathBuf,
        args: Vec<OsString>,
        envs: Vec<(OsString, OsString)>,
    },
}

impl WorkerLauncher {
    fn agent_profile(exe: PathBuf) -> Self {
        Self::AgentProfile { exe }
    }

    #[cfg(test)]
    fn custom(exe: PathBuf, args: Vec<OsString>, envs: Vec<(OsString, OsString)>) -> Self {
        Self::Custom { exe, args, envs }
    }

    fn command(&self, profile: &ResolvedProfile) -> TokioCommand {
        match self {
            Self::AgentProfile { exe } => {
                let mut command = TokioCommand::new(exe);
                command
                    .arg("agent")
                    .arg("run")
                    .arg("--agent-profile")
                    .arg(&profile.path);
                command
            }
            #[cfg(test)]
            Self::Custom { exe, args, envs } => {
                let mut command = TokioCommand::new(exe);
                command.args(args);
                for (key, value) in envs {
                    command.env(key, value);
                }
                command
            }
        }
    }
}

async fn oldest_worker_key(
    workers: &HashMap<String, Arc<WorkerProcess>>,
    exclude: Option<&str>,
) -> Option<String> {
    let mut oldest: Option<(String, Instant)> = None;
    for (id, worker) in workers {
        if exclude == Some(id.as_str()) {
            continue;
        }
        if worker.is_busy().await {
            continue;
        }
        let last_used = worker.last_used().await;
        if oldest
            .as_ref()
            .is_none_or(|(_, oldest_last_used)| last_used < *oldest_last_used)
        {
            oldest = Some((id.clone(), last_used));
        }
    }
    oldest.map(|(id, _)| id)
}

async fn stop_worker(worker: Arc<WorkerProcess>) -> Result<()> {
    let _ = worker.request("_dwo/worker/shutdown", json!({})).await;
    let mut child = worker.child.lock().await;
    if child.try_wait()?.is_none() {
        let _ = child.kill().await;
    }
    Ok(())
}

struct WorkerProcess {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<WorkerMessage, String>>>>>,
    events: broadcast::Sender<Value>,
    last_used: Mutex<Instant>,
    in_flight: Mutex<usize>,
}

impl WorkerProcess {
    async fn spawn(launcher: &WorkerLauncher, profile: &ResolvedProfile) -> Result<Self> {
        let mut command = launcher.command(profile);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn worker process")?;
        let stdin = child.stdin.take().context("worker stdin was not piped")?;
        let stdout = child.stdout.take().context("worker stdout was not piped")?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1024);
        spawn_worker_stdout_reader(profile.id.clone(), stdout, pending.clone(), events.clone());
        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            events,
            last_used: Mutex::new(Instant::now()),
            in_flight: Mutex::new(0),
        })
    }

    async fn child_id(&self) -> Option<u32> {
        self.child.lock().await.id()
    }

    async fn touch(&self) {
        *self.last_used.lock().await = Instant::now();
    }

    async fn last_used(&self) -> Instant {
        *self.last_used.lock().await
    }

    async fn is_busy(&self) -> bool {
        *self.in_flight.lock().await > 0
    }

    async fn begin_request(&self) {
        let mut in_flight = self.in_flight.lock().await;
        *in_flight += 1;
    }

    async fn finish_request(&self) {
        let mut in_flight = self.in_flight.lock().await;
        *in_flight = in_flight.saturating_sub(1);
        drop(in_flight);
        self.touch().await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.begin_request().await;
        let result = self.request_locked(method, params).await;
        self.finish_request().await;
        result
    }

    async fn request_locked(&self, method: &str, params: Value) -> Result<Value> {
        let id = Uuid::new_v4().to_string();
        let request = WorkerRequest {
            jsonrpc: "2.0",
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), response_tx);
        let mut line = serde_json::to_vec(&request).context("serialize worker request")?;
        line.push(b'\n');
        let write_result = async {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(&line)
                .await
                .context("write worker request")?;
            stdin.flush().await.context("flush worker request")?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(err) = write_result {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }

        let response = response_rx
            .await
            .context("worker response channel closed")?
            .map_err(anyhow::Error::msg)?;
        if response.id.as_ref() != Some(&Value::String(id.clone())) {
            bail!("worker response id mismatch");
        }
        if let Some(error) = response.error {
            bail!("{}", worker_error_message(&error));
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_vec(&request).context("serialize worker notification")?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&line)
            .await
            .context("write worker notification")?;
        stdin.flush().await.context("flush worker notification")?;
        Ok(())
    }

    async fn forward_client_response(&self, response: Value) -> Result<()> {
        let mut line = serde_json::to_vec(&response).context("serialize client response")?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&line)
            .await
            .context("write client response to worker")?;
        stdin
            .flush()
            .await
            .context("flush client response to worker")?;
        Ok(())
    }

    fn subscribe_events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }
}

fn spawn_worker_stdout_reader(
    profile_id: String,
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<WorkerMessage, String>>>>>,
    events: broadcast::Sender<Value>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let response_line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(err) => {
                    fail_pending_responses(&pending, format!("read worker response: {err}")).await;
                    return;
                }
            };
            let response: WorkerMessage = match serde_json::from_str(&response_line) {
                Ok(response) => response,
                Err(err) => {
                    tracing::warn!(
                        profile = %profile_id,
                        error = %err,
                        "failed to parse worker stdout message"
                    );
                    continue;
                }
            };
            if let Some(method) = response.method {
                let params = response.params.unwrap_or(Value::Null);
                let event = if let Some(worker_request_id) = response.id {
                    json!({
                        "type": "client_request",
                        "id": pending_client_request_id(&profile_id, &worker_request_id),
                        "method": method,
                        "params": params,
                    })
                } else {
                    json!({
                        "method": method,
                        "params": params,
                    })
                };
                let _ = events.send(event);
                continue;
            }
            let Some(id) = response.id.as_ref().map(worker_response_key) else {
                tracing::warn!(profile = %profile_id, "worker response without id");
                continue;
            };
            let sender = pending.lock().await.remove(&id);
            match sender {
                Some(sender) => {
                    let _ = sender.send(Ok(response));
                }
                None => {
                    tracing::warn!(profile = %profile_id, response_id = %id, "unexpected worker response id");
                }
            }
        }
        fail_pending_responses(&pending, "worker exited before responding".to_string()).await;
    });
}

async fn fail_pending_responses(
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Result<WorkerMessage, String>>>>>,
    error: String,
) {
    let pending = {
        let mut guard = pending.lock().await;
        std::mem::take(&mut *guard)
    };
    for (_, sender) in pending {
        let _ = sender.send(Err(error.clone()));
    }
}

fn worker_response_key(id: &Value) -> String {
    id.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string())
}

fn worker_error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| error.to_string())
}

fn pending_client_request_id(profile_id: &str, worker_request_id: &Value) -> String {
    let encoded_profile = URL_SAFE_NO_PAD.encode(profile_id.as_bytes());
    let encoded_id = URL_SAFE_NO_PAD.encode(worker_request_id.to_string().as_bytes());
    format!("_dwo_worker_client_request:{encoded_profile}:{encoded_id}")
}

fn pending_client_request_parts(request_id: &Value) -> Option<(String, Value)> {
    let id = request_id.as_str()?;
    let rest = id.strip_prefix("_dwo_worker_client_request:")?;
    let (encoded_profile, encoded_worker_id) = rest.split_once(':')?;
    let profile_bytes = URL_SAFE_NO_PAD.decode(encoded_profile).ok()?;
    let worker_id_bytes = URL_SAFE_NO_PAD.decode(encoded_worker_id).ok()?;
    let profile_id = String::from_utf8(profile_bytes).ok()?;
    let worker_request_id = serde_json::from_slice(&worker_id_bytes).ok()?;
    Some((profile_id, worker_request_id))
}

async fn handle_supervisor_connection(
    stream: tokio::net::TcpStream,
    state: Arc<SupervisorState>,
) -> Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let websocket = tokio_tungstenite::accept_async(stream)
        .await
        .context("accept supervisor websocket")?;
    let (write, mut read) = websocket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let connection_id = Uuid::new_v4().to_string();
    let connection = SupervisorConnectionContext {
        id: connection_id.clone(),
        sender: out_tx.clone(),
    };
    let writer_task = tokio::spawn(async move {
        let mut write = write;
        while let Some(value) = out_rx.recv().await {
            write
                .send(Message::Text(serde_json::to_string(&value)?))
                .await
                .context("send supervisor websocket message")?;
        }
        Ok::<(), anyhow::Error>(())
    });
    out_tx
        .send(json!({
            "type": "supervisor.ready",
            "protocol": "dwo-supervisor-v1",
        }))
        .context("queue supervisor hello")?;
    while let Some(message) = read.next().await {
        let message = message.context("read supervisor websocket message")?;
        if message.is_close() {
            break;
        }
        if !message.is_text() && !message.is_binary() {
            continue;
        }
        let text = message
            .into_text()
            .context("read supervisor message text")?;
        let request = match serde_json::from_str::<SupervisorRequest>(&text) {
            Ok(request) => request,
            Err(err) => {
                let response = SupervisorResponse::error(None, format!("invalid request: {err}"));
                out_tx
                    .send(serde_json::to_value(response)?)
                    .context("queue supervisor response")?;
                continue;
            }
        };

        let state = state.clone();
        let out_for_response = out_tx.clone();
        let out_for_events = out_tx.clone();
        let connection_for_request = connection.clone();
        tokio::spawn(async move {
            let response = state
                .handle_request_with_events(request, Some(connection_for_request), move |event| {
                    let out = out_for_events.clone();
                    async move { out.send(event).context("queue supervisor event") }
                })
                .await;

            if let Err(err) = out_for_response
                .send(serde_json::to_value(response).unwrap_or_else(|err| {
                    json!({
                        "type": "supervisor.error",
                        "error": format!("serialize supervisor response failed: {err}"),
                    })
                }))
                .context("queue supervisor response")
            {
                tracing::warn!(error = %format!("{err:#}"), "send supervisor response failed");
            }
        });
    }
    state.event_bus.unregister_connection(&connection_id).await;
    drop(out_tx);
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(error = %format!("{err:#}"), "supervisor writer task failed");
        }
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "supervisor writer task panicked");
        }
    }
    Ok(())
}

pub fn enable_supervisor() -> Result<()> {
    let exe = env::current_exe().context("resolve current dwoagent executable")?;
    if cfg!(windows) {
        register_windows_supervisor(&exe)
    } else if cfg!(target_os = "macos") {
        register_macos_supervisor(&exe)
    } else if cfg!(target_os = "linux") {
        register_linux_supervisor(&exe)
    } else {
        bail!("supervisor startup is not supported on this platform")
    }
}

pub fn start_supervisor() -> Result<bool> {
    if !supervisor_processes()?.is_empty() {
        return Ok(false);
    }

    let exe = env::current_exe().context("resolve current dwoagent executable")?;
    if cfg!(windows) {
        start_windows_supervisor(&exe)?;
    } else {
        start_unix_supervisor(&exe)?;
    }
    std::thread::sleep(Duration::from_millis(200));
    Ok(true)
}

pub fn stop_supervisor() -> Result<usize> {
    let processes = supervisor_processes()?;
    for process in &processes {
        stop_process(process.pid)?;
    }
    Ok(processes.len())
}

pub fn disable_supervisor() -> Result<bool> {
    if cfg!(windows) {
        unregister_windows_supervisor()
    } else if cfg!(target_os = "macos") {
        unregister_macos_supervisor()
    } else if cfg!(target_os = "linux") {
        unregister_linux_supervisor()
    } else {
        bail!("supervisor startup is not supported on this platform")
    }
}

pub fn supervisor_status() -> Result<String> {
    let startup = if cfg!(windows) {
        windows_supervisor_status()?
    } else if cfg!(target_os = "macos") {
        macos_supervisor_status()?
    } else if cfg!(target_os = "linux") {
        linux_supervisor_status()?
    } else {
        None
    };
    let processes = supervisor_processes()?;
    let running = if processes.is_empty() {
        "not running".to_string()
    } else {
        format!(
            "running pid(s): {}",
            processes
                .iter()
                .map(|process| process.pid.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let startup = startup.unwrap_or_else(|| "startup not registered".to_string());
    Ok(format!("{startup}; {running}"))
}

fn supervisor_processes() -> Result<Vec<SupervisorProcess>> {
    if cfg!(windows) {
        windows_supervisor_processes()
    } else {
        unix_supervisor_processes()
    }
}

fn start_windows_supervisor(exe: &Path) -> Result<()> {
    let launcher = write_windows_supervisor_launcher(exe)?;
    let status = Command::new("wscript.exe")
        .args(["//B", "//Nologo"])
        .arg(launcher)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("start Windows supervisor launcher")?;
    if !status.success() {
        bail!("start Windows supervisor launcher failed");
    }
    Ok(())
}

fn start_unix_supervisor(exe: &Path) -> Result<()> {
    Command::new(exe)
        .args(["supervisor", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start supervisor process")?;
    Ok(())
}

fn register_windows_supervisor(exe: &Path) -> Result<()> {
    let launcher = write_windows_supervisor_launcher(exe)?;
    let task_command = format!("wscript.exe //B //Nologo \"{}\"", launcher.display());
    let status = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            WINDOWS_TASK_NAME,
            "/SC",
            "ONLOGON",
            "/TR",
            &task_command,
            "/RL",
            "LIMITED",
            "/F",
        ])
        .status()
        .context("register Windows scheduled task")?;
    if !status.success() {
        bail!("register Windows scheduled task failed");
    }
    Ok(())
}

fn write_windows_supervisor_launcher(exe: &Path) -> Result<PathBuf> {
    let path = windows_supervisor_launcher_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let script = format!(
        r#"Option Explicit

Dim shell
Dim exePath
Dim commandLine

Set shell = CreateObject("WScript.Shell")
exePath = "{}"
commandLine = """" & exePath & """ supervisor run"

If Not IsDwoAgentSupervisorRunning() Then
  shell.Run commandLine, 0, False
End If

Function IsDwoAgentSupervisorRunning()
  Dim wmi
  Dim processes
  Dim process
  Dim command

  IsDwoAgentSupervisorRunning = False
  Set wmi = GetObject("winmgmts:\\.\root\cimv2")
  Set processes = wmi.ExecQuery("SELECT CommandLine FROM Win32_Process")

  For Each process In processes
    command = process.CommandLine
    If Not IsNull(command) Then
      If InStr(1, command, exePath, 1) > 0 And InStr(1, command, "supervisor run", 1) > 0 Then
        IsDwoAgentSupervisorRunning = True
        Exit Function
      End If
    End If
  Next
End Function
"#,
        vbs_escape(&exe.display().to_string())
    );
    fs::write(&path, script).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn unregister_windows_supervisor() -> Result<bool> {
    let output = Command::new("schtasks")
        .args(["/Query", "/TN", WINDOWS_TASK_NAME])
        .output()
        .context("query Windows scheduled task")?;
    if !output.status.success() {
        return Ok(false);
    }
    let status = Command::new("schtasks")
        .args(["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"])
        .status()
        .context("delete Windows scheduled task")?;
    if !status.success() {
        bail!("delete Windows scheduled task failed");
    }
    let launcher = windows_supervisor_launcher_path()?;
    if launcher.is_file() {
        fs::remove_file(&launcher).with_context(|| format!("remove {}", launcher.display()))?;
    }
    Ok(true)
}

fn windows_supervisor_status() -> Result<Option<String>> {
    let output = Command::new("schtasks")
        .args(["/Query", "/TN", WINDOWS_TASK_NAME])
        .output()
        .context("query Windows scheduled task")?;
    if output.status.success() {
        Ok(Some(format!(
            "startup registered: Windows scheduled task `{WINDOWS_TASK_NAME}`"
        )))
    } else {
        Ok(None)
    }
}

fn windows_supervisor_processes() -> Result<Vec<SupervisorProcess>> {
    let script = "$selfPid = $PID; \
       $matches = foreach ($process in Get-CimInstance Win32_Process) { \
         $command = $process.CommandLine; \
         $exe = $process.ExecutablePath; \
         if ($process.ProcessId -ne $selfPid -and \
             $null -ne $command -and \
             $null -ne $exe -and \
             [IO.Path]::GetFileName($exe).Equals('dwoagent.exe', [StringComparison]::OrdinalIgnoreCase) -and \
             $command.IndexOf('supervisor run', [StringComparison]::OrdinalIgnoreCase) -ge 0) { \
           [pscustomobject]@{ pid = [int]$process.ProcessId; command_line = $command }; \
         } \
       }; \
       $matches | ConvertTo-Json -Compress";
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
        .context("query Windows supervisor processes")?;
    if !output.status.success() {
        bail!("query Windows supervisor processes failed");
    }
    parse_process_json(&String::from_utf8_lossy(&output.stdout))
}

fn register_macos_supervisor(exe: &Path) -> Result<()> {
    let path = macos_launch_agent_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>supervisor</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
"#,
        xml_escape(MACOS_LAUNCH_AGENT_ID),
        xml_escape(&exe.display().to_string())
    );
    fs::write(&path, plist).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn unregister_macos_supervisor() -> Result<bool> {
    let path = macos_launch_agent_path()?;
    if path.is_file() {
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        return Ok(true);
    }
    Ok(false)
}

fn macos_supervisor_status() -> Result<Option<String>> {
    let path = macos_launch_agent_path()?;
    if path.is_file() {
        Ok(Some(format!("startup registered: {}", path.display())))
    } else {
        Ok(None)
    }
}

fn register_linux_supervisor(exe: &Path) -> Result<()> {
    if find_executable("systemctl").is_some() {
        register_linux_systemd_supervisor(exe)
    } else {
        register_linux_desktop_autostart(exe)
    }
}

fn unregister_linux_supervisor() -> Result<bool> {
    let mut removed = false;
    let systemd_path = linux_systemd_unit_path()?;
    if systemd_path.is_file() {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", LINUX_SYSTEMD_UNIT])
            .status();
        fs::remove_file(&systemd_path)
            .with_context(|| format!("remove {}", systemd_path.display()))?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        removed = true;
    }
    let desktop_path = linux_desktop_autostart_path()?;
    if desktop_path.is_file() {
        fs::remove_file(&desktop_path)
            .with_context(|| format!("remove {}", desktop_path.display()))?;
        removed = true;
    }
    Ok(removed)
}

fn linux_supervisor_status() -> Result<Option<String>> {
    let systemd_path = linux_systemd_unit_path()?;
    if systemd_path.is_file() {
        return Ok(Some(format!(
            "startup registered: {}",
            systemd_path.display()
        )));
    }
    let desktop_path = linux_desktop_autostart_path()?;
    if desktop_path.is_file() {
        return Ok(Some(format!(
            "startup registered: {}",
            desktop_path.display()
        )));
    }
    Ok(None)
}

fn register_linux_systemd_supervisor(exe: &Path) -> Result<()> {
    let path = linux_systemd_unit_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let unit = format!(
        r#"[Unit]
Description=DwoAgent supervisor

[Service]
ExecStart={} supervisor run
Restart=always

[Install]
WantedBy=default.target
"#,
        exe.display()
    );
    fs::write(&path, unit).with_context(|| format!("write {}", path.display()))?;
    let status = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("reload user systemd")?;
    if !status.success() {
        bail!("systemctl --user daemon-reload failed");
    }
    let status = Command::new("systemctl")
        .args(["--user", "enable", LINUX_SYSTEMD_UNIT])
        .status()
        .context("enable user systemd supervisor")?;
    if !status.success() {
        bail!("systemctl --user enable {LINUX_SYSTEMD_UNIT} failed");
    }
    Ok(())
}

fn register_linux_desktop_autostart(exe: &Path) -> Result<()> {
    let path = linux_desktop_autostart_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=DwoAgent Supervisor\nExec={} supervisor run\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    );
    fs::write(&path, desktop).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn unix_supervisor_processes() -> Result<Vec<SupervisorProcess>> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,command="])
        .output()
        .context("query supervisor processes with ps")?;
    if !output.status.success() {
        bail!("query supervisor processes failed");
    }
    let mut processes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim_start();
        let Some((pid, command_line)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        if !command_line.contains("supervisor run") {
            continue;
        }
        let Ok(pid) = pid.trim().parse::<u32>() else {
            continue;
        };
        processes.push(SupervisorProcess {
            pid,
            command_line: command_line.to_string(),
        });
    }
    Ok(processes)
}

fn parse_process_json(text: &str) -> Result<Vec<SupervisorProcess>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).context("parse process query JSON")?;
    match value {
        serde_json::Value::Array(_) => {
            serde_json::from_value(value).context("deserialize process query JSON array")
        }
        serde_json::Value::Object(_) => Ok(vec![
            serde_json::from_value(value).context("deserialize process query JSON object")?,
        ]),
        _ => Ok(Vec::new()),
    }
}

fn stop_process(pid: u32) -> Result<()> {
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .context("stop Windows supervisor process")?
    } else {
        Command::new("kill")
            .arg(pid.to_string())
            .status()
            .context("stop supervisor process")?
    };
    if !status.success() {
        bail!("stop supervisor process {pid} failed");
    }
    Ok(())
}

fn default_version() -> u32 {
    1
}

fn default_websocket_bind_addr() -> String {
    "127.0.0.1:8766".to_string()
}

fn default_max_workers() -> usize {
    3
}

fn default_idle_seconds() -> u64 {
    600
}

fn generate_secret() -> String {
    let uuid = Uuid::new_v4();
    format!("dwo_sup_{}", URL_SAFE_NO_PAD.encode(uuid.as_bytes()))
}

fn windows_supervisor_launcher_path() -> Result<PathBuf> {
    let base = match env::var_os("APPDATA") {
        Some(appdata) => PathBuf::from(appdata),
        None => home_dir()?.join("AppData").join("Roaming"),
    };
    Ok(base.join("DwoAgent").join(WINDOWS_LAUNCHER_FILE))
}

fn macos_launch_agent_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{MACOS_LAUNCH_AGENT_ID}.plist")))
}

fn linux_systemd_unit_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".config")
        .join("systemd")
        .join("user")
        .join(LINUX_SYSTEMD_UNIT))
}

fn linux_desktop_autostart_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".config")
        .join("autostart")
        .join(LINUX_DESKTOP_FILE))
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .context("cannot resolve home directory")
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(name);
    if candidate.components().count() > 1 && candidate.is_file() {
        return Some(candidate);
    }
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        for candidate in executable_candidates(&dir, name) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let base = dir.join(name);
    if cfg!(windows) {
        let path = Path::new(name);
        if path.extension().is_some() {
            return vec![base];
        }
        let pathext =
            env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
        let mut candidates = vec![base.clone()];
        for ext in pathext.to_string_lossy().split(';') {
            candidates.push(dir.join(format!("{name}{ext}")));
        }
        candidates
    } else {
        vec![base]
    }
}

fn vbs_escape(value: &str) -> String {
    value.replace('"', "\"\"")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn notify_reaches_worker_while_request_is_running() -> Result<()> {
        let (fake_worker_exe, fake_worker_args, fake_worker_script) = fake_worker_command()?;
        let profile_id = "test-profile".to_string();
        let secret = "test-secret".to_string();
        let event_bus = Arc::new(SessionEventBus::new());
        let config_cache = Arc::new(SessionConfigCache::new());
        let permissions = Arc::new(SupervisorPermissionRegistry::new());
        let state = Arc::new(SupervisorState {
            config: SupervisorConfig {
                endpoint: SupervisorEndpointConfig {
                    websocket_bind_addr: "127.0.0.1:0".to_string(),
                    secret: secret.clone(),
                },
                profiles: vec![SupervisorProfileConfig {
                    id: profile_id.clone(),
                    path: PathBuf::from("unused-profile-path"),
                }],
                pool: SupervisorPoolConfig {
                    max_workers: 1,
                    idle_seconds: 60,
                },
                ..SupervisorConfig::default()
            },
            worker_pool: WorkerPool::new_with_launcher(
                WorkerLauncher::custom(fake_worker_exe, fake_worker_args, Vec::new()),
                event_bus.clone(),
                config_cache.clone(),
                permissions.clone(),
            ),
            event_bus,
            config_cache,
            permissions,
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_state = state.clone();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            handle_supervisor_connection(stream, server_state).await
        });

        let stream = tokio::net::TcpStream::connect(addr).await?;
        let url = format!("ws://{addr}");
        let (mut websocket, _) = tokio_tungstenite::client_async(url, stream).await?;
        let ready = websocket
            .next()
            .await
            .context("supervisor did not send ready")??;
        assert!(ready.is_text());

        websocket
            .send(Message::Text(
                json!({
                    "id": "prompt",
                    "type": "worker.request",
                    "secret": secret,
                    "profile": profile_id,
                    "method": "session/prompt",
                    "params": { "sessionId": "s1" },
                })
                .to_string(),
            ))
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        websocket
            .send(Message::Text(
                json!({
                    "id": "cancel",
                    "type": "worker.notify",
                    "secret": "test-secret",
                    "profile": "test-profile",
                    "method": "session/cancel",
                    "params": { "sessionId": "s1" },
                })
                .to_string(),
            ))
            .await?;

        let mut saw_cancel_ack = false;
        let mut saw_prompt_cancelled = false;
        timeout(Duration::from_secs(3), async {
            while !saw_prompt_cancelled {
                let message = websocket
                    .next()
                    .await
                    .context("websocket closed before prompt response")??;
                if !message.is_text() {
                    continue;
                }
                let value: Value = serde_json::from_str(message.to_text()?)?;
                if value.get("type").and_then(Value::as_str) != Some("supervisor.result") {
                    continue;
                }
                match value.get("id").and_then(Value::as_str) {
                    Some("cancel") => {
                        saw_cancel_ack = true;
                    }
                    Some("prompt") => {
                        assert_eq!(
                            value
                                .pointer("/result/result/stopReason")
                                .and_then(Value::as_str),
                            Some("cancelled")
                        );
                        saw_prompt_cancelled = true;
                    }
                    _ => {}
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("cancel notification was blocked behind the running prompt")??;

        assert!(
            saw_cancel_ack,
            "supervisor should acknowledge worker.notify"
        );
        websocket.close(None).await?;
        state.worker_pool.shutdown_all().await;
        let _ = timeout(Duration::from_secs(1), server_task).await;
        let _ = std::fs::remove_file(fake_worker_script);
        Ok(())
    }

    #[tokio::test]
    async fn load_and_other_session_prompt_do_not_wait_for_running_prompt() -> Result<()> {
        let (fake_worker_exe, fake_worker_args, fake_worker_script) =
            fake_concurrent_worker_command()?;
        let profile_id = "test-profile".to_string();
        let secret = "test-secret".to_string();
        let event_bus = Arc::new(SessionEventBus::new());
        let config_cache = Arc::new(SessionConfigCache::new());
        let permissions = Arc::new(SupervisorPermissionRegistry::new());
        let state = Arc::new(SupervisorState {
            config: SupervisorConfig {
                endpoint: SupervisorEndpointConfig {
                    websocket_bind_addr: "127.0.0.1:0".to_string(),
                    secret: secret.clone(),
                },
                profiles: vec![SupervisorProfileConfig {
                    id: profile_id.clone(),
                    path: PathBuf::from("unused-profile-path"),
                }],
                pool: SupervisorPoolConfig {
                    max_workers: 1,
                    idle_seconds: 60,
                },
                ..SupervisorConfig::default()
            },
            worker_pool: WorkerPool::new_with_launcher(
                WorkerLauncher::custom(fake_worker_exe, fake_worker_args, Vec::new()),
                event_bus.clone(),
                config_cache.clone(),
                permissions.clone(),
            ),
            event_bus,
            config_cache,
            permissions,
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_state = state.clone();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            handle_supervisor_connection(stream, server_state).await
        });

        let stream = tokio::net::TcpStream::connect(addr).await?;
        let url = format!("ws://{addr}");
        let (mut websocket, _) = tokio_tungstenite::client_async(url, stream).await?;
        assert!(websocket.next().await.context("missing ready")??.is_text());

        websocket
            .send(Message::Text(
                json!({
                    "id": "prompt-s1",
                    "type": "worker.request",
                    "secret": secret,
                    "profile": profile_id,
                    "method": "session/prompt",
                    "params": { "sessionId": "s1" },
                })
                .to_string(),
            ))
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        websocket
            .send(Message::Text(
                json!({
                    "id": "load-s1",
                    "type": "worker.request",
                    "secret": "test-secret",
                    "profile": "test-profile",
                    "method": "session/load",
                    "params": { "sessionId": "s1" },
                })
                .to_string(),
            ))
            .await?;
        let load = wait_supervisor_result(&mut websocket, "load-s1").await?;
        assert_eq!(
            load.pointer("/result/result/sessionId")
                .and_then(Value::as_str),
            Some("s1")
        );

        websocket
            .send(Message::Text(
                json!({
                    "id": "prompt-s2",
                    "type": "worker.request",
                    "secret": "test-secret",
                    "profile": "test-profile",
                    "method": "session/prompt",
                    "params": { "sessionId": "s2" },
                })
                .to_string(),
            ))
            .await?;
        let prompt_s2 = wait_supervisor_result(&mut websocket, "prompt-s2").await?;
        assert_eq!(
            prompt_s2
                .pointer("/result/result/stopReason")
                .and_then(Value::as_str),
            Some("end_turn")
        );

        let _ = websocket.close(None).await;
        state.worker_pool.shutdown_all().await;
        let _ = timeout(Duration::from_secs(1), server_task).await;
        let _ = std::fs::remove_file(fake_worker_script);
        Ok(())
    }

    #[tokio::test]
    async fn session_updates_fan_out_to_observers() -> Result<()> {
        let (fake_worker_exe, fake_worker_args, fake_worker_script) = fake_event_worker_command()?;
        let profile_id = "test-profile".to_string();
        let secret = "test-secret".to_string();
        let event_bus = Arc::new(SessionEventBus::new());
        let config_cache = Arc::new(SessionConfigCache::new());
        let permissions = Arc::new(SupervisorPermissionRegistry::new());
        let state = Arc::new(SupervisorState {
            config: SupervisorConfig {
                endpoint: SupervisorEndpointConfig {
                    websocket_bind_addr: "127.0.0.1:0".to_string(),
                    secret: secret.clone(),
                },
                profiles: vec![SupervisorProfileConfig {
                    id: profile_id.clone(),
                    path: PathBuf::from("unused-profile-path"),
                }],
                pool: SupervisorPoolConfig {
                    max_workers: 1,
                    idle_seconds: 60,
                },
                ..SupervisorConfig::default()
            },
            worker_pool: WorkerPool::new_with_launcher(
                WorkerLauncher::custom(fake_worker_exe, fake_worker_args, Vec::new()),
                event_bus.clone(),
                config_cache.clone(),
                permissions.clone(),
            ),
            event_bus,
            config_cache,
            permissions,
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_state = state.clone();
        let server_task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await?;
                let state = server_state.clone();
                tokio::spawn(async move {
                    let _ = handle_supervisor_connection(stream, state).await;
                });
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        let url = format!("ws://{addr}");
        let stream_a = tokio::net::TcpStream::connect(addr).await?;
        let (mut observer, _) = tokio_tungstenite::client_async(url.clone(), stream_a).await?;
        assert!(
            observer
                .next()
                .await
                .context("missing observer ready")??
                .is_text()
        );
        let stream_b = tokio::net::TcpStream::connect(addr).await?;
        let (mut trigger, _) = tokio_tungstenite::client_async(url, stream_b).await?;
        assert!(
            trigger
                .next()
                .await
                .context("missing trigger ready")??
                .is_text()
        );

        observer
            .send(Message::Text(
                json!({
                    "id": "load",
                    "type": "worker.request",
                    "secret": secret,
                    "profile": profile_id,
                    "method": "session/load",
                    "params": { "sessionId": "s1" },
                })
                .to_string(),
            ))
            .await?;
        wait_supervisor_result(&mut observer, "load").await?;

        trigger
            .send(Message::Text(
                json!({
                    "id": "remote",
                    "type": "worker.request",
                    "secret": "test-secret",
                    "profile": "test-profile",
                    "method": "_dwo/ingress/handle_event",
                    "params": {},
                })
                .to_string(),
            ))
            .await?;

        let event = wait_supervisor_event(&mut observer, "session/update").await?;
        assert_eq!(
            event
                .pointer("/event/params/sessionId")
                .and_then(Value::as_str),
            Some("s1")
        );
        assert_eq!(
            event
                .pointer("/event/params/update/content/text")
                .and_then(Value::as_str),
            Some("remote")
        );

        let _ = trigger.close(None).await;
        let _ = observer.close(None).await;
        server_task.abort();
        state.worker_pool.shutdown_all().await;
        let _ = std::fs::remove_file(fake_worker_script);
        Ok(())
    }

    #[tokio::test]
    async fn user_prompt_update_is_not_echoed_to_prompt_origin() -> Result<()> {
        let event_bus = SessionEventBus::new();
        let (origin_tx, mut origin_rx) = mpsc::unbounded_channel();
        let (observer_tx, mut observer_rx) = mpsc::unbounded_channel();
        let origin = SupervisorConnectionContext {
            id: "origin".to_string(),
            sender: origin_tx,
        };
        let observer = SupervisorConnectionContext {
            id: "observer".to_string(),
            sender: observer_tx,
        };
        event_bus.subscribe(&origin, "profile", "s1").await;
        event_bus.subscribe(&observer, "profile", "s1").await;
        let token = event_bus
            .begin_prompt_origin(&origin, "profile", "s1")
            .await
            .context("prompt origin token")?;

        let user_event = json!({
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "user_message_chunk",
                    "content": { "type": "text", "text": "hello" },
                },
            },
        });
        event_bus
            .broadcast_worker_message("profile", &user_event)
            .await;

        let observed = timeout(Duration::from_secs(1), observer_rx.recv())
            .await?
            .context("observer should receive user prompt update")?;
        assert_eq!(
            observed
                .pointer("/event/params/update/content/text")
                .and_then(Value::as_str),
            Some("hello")
        );
        assert!(
            timeout(Duration::from_millis(100), origin_rx.recv())
                .await
                .is_err(),
            "origin should not receive its own user prompt update"
        );

        let assistant_event = json!({
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "model" },
                },
            },
        });
        event_bus
            .broadcast_worker_message("profile", &assistant_event)
            .await;
        let origin_message = timeout(Duration::from_secs(1), origin_rx.recv())
            .await?
            .context("origin should receive assistant update")?;
        let observer_message = timeout(Duration::from_secs(1), observer_rx.recv())
            .await?
            .context("observer should receive assistant update")?;
        assert_eq!(
            origin_message
                .pointer("/event/params/update/content/text")
                .and_then(Value::as_str),
            Some("model")
        );
        assert_eq!(
            observer_message
                .pointer("/event/params/update/content/text")
                .and_then(Value::as_str),
            Some("model")
        );

        event_bus
            .end_prompt_origin("profile", "s1", token.as_str())
            .await;
        Ok(())
    }

    #[tokio::test]
    async fn permission_registry_invalidates_pending_request_for_new_prompt() -> Result<()> {
        let registry = SupervisorPermissionRegistry::new();
        let event = json!({
            "type": "client_request",
            "id": "_dwo_worker_client_request:profile:req1",
            "method": "session/request_permission",
            "params": {
                "sessionId": "s1",
                "toolCall": {
                    "toolCallId": "tool1",
                    "title": "file_edit",
                    "rawInput": {"path": "a.txt"}
                },
                "options": []
            }
        });
        let pending = registry
            .register_client_request("profile", &event)
            .await
            .context("permission should register")?;
        assert_eq!(pending.session_id, "s1");

        registry.invalidate_session("profile", "s1").await;
        let should_forward = registry
            .should_forward_client_response(&json!({
                "jsonrpc": "2.0",
                "id": "_dwo_worker_client_request:profile:req1",
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_once"
                    }
                }
            }))
            .await;
        assert!(!should_forward);
        Ok(())
    }

    #[tokio::test]
    async fn permission_registry_consumes_confirmation_and_marks_client_response_stale()
    -> Result<()> {
        let registry = SupervisorPermissionRegistry::new();
        let event = json!({
            "type": "client_request",
            "id": "_dwo_worker_client_request:profile:req2",
            "method": "session/request_permission",
            "params": {
                "sessionId": "s1",
                "toolCall": {
                    "toolCallId": "tool2",
                    "title": "shell",
                    "rawInput": {"command": "date"}
                },
                "options": []
            }
        });
        let pending = registry
            .register_client_request("profile", &event)
            .await
            .context("permission should register")?;

        let taken = registry
            .take_confirmation(&pending.confirmation_id)
            .await
            .context("confirmation should be consumable")?;
        assert_eq!(taken.request_id, pending.request_id);
        assert!(
            registry
                .is_stale_confirmation(&pending.confirmation_id)
                .await
        );
        assert!(
            registry
                .snapshot_by_request_id(&pending.request_id)
                .await
                .is_none()
        );

        let should_forward = registry
            .should_forward_client_response(&json!({
                "jsonrpc": "2.0",
                "id": "_dwo_worker_client_request:profile:req2",
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_once"
                    }
                }
            }))
            .await;
        assert!(!should_forward);
        Ok(())
    }

    #[tokio::test]
    async fn config_request_is_acknowledged_with_cached_options_while_prompt_is_running()
    -> Result<()> {
        let (fake_worker_exe, fake_worker_args, fake_worker_script) =
            fake_control_worker_command()?;
        let profile_id = "test-profile".to_string();
        let secret = "test-secret".to_string();
        let event_bus = Arc::new(SessionEventBus::new());
        let config_cache = Arc::new(SessionConfigCache::new());
        let permissions = Arc::new(SupervisorPermissionRegistry::new());
        let state = Arc::new(SupervisorState {
            config: SupervisorConfig {
                endpoint: SupervisorEndpointConfig {
                    websocket_bind_addr: "127.0.0.1:0".to_string(),
                    secret: secret.clone(),
                },
                profiles: vec![SupervisorProfileConfig {
                    id: profile_id.clone(),
                    path: PathBuf::from("unused-profile-path"),
                }],
                pool: SupervisorPoolConfig {
                    max_workers: 1,
                    idle_seconds: 60,
                },
                ..SupervisorConfig::default()
            },
            worker_pool: WorkerPool::new_with_launcher(
                WorkerLauncher::custom(fake_worker_exe, fake_worker_args, Vec::new()),
                event_bus.clone(),
                config_cache.clone(),
                permissions.clone(),
            ),
            event_bus,
            config_cache,
            permissions,
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_state = state.clone();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            handle_supervisor_connection(stream, server_state).await
        });

        let stream = tokio::net::TcpStream::connect(addr).await?;
        let url = format!("ws://{addr}");
        let (mut websocket, _) = tokio_tungstenite::client_async(url, stream).await?;
        assert!(websocket.next().await.context("missing ready")??.is_text());

        websocket
            .send(Message::Text(
                json!({
                    "id": "new",
                    "type": "worker.request",
                    "secret": secret,
                    "profile": profile_id,
                    "method": "session/new",
                    "params": {},
                })
                .to_string(),
            ))
            .await?;
        wait_supervisor_result(&mut websocket, "new").await?;

        websocket
            .send(Message::Text(
                json!({
                    "id": "prompt",
                    "type": "worker.request",
                    "secret": secret,
                    "profile": profile_id,
                    "method": "session/prompt",
                    "params": { "sessionId": "s1" },
                })
                .to_string(),
            ))
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        websocket
            .send(Message::Text(
                json!({
                    "id": "model",
                    "type": "worker.request",
                    "secret": "test-secret",
                    "profile": "test-profile",
                    "method": "session/set_config_option",
                    "params": {
                        "sessionId": "s1",
                        "configId": "model",
                        "value": "mock-flash"
                    },
                })
                .to_string(),
            ))
            .await?;

        let response = wait_supervisor_result(&mut websocket, "model").await?;
        let config_options = response
            .pointer("/result/result/configOptions")
            .and_then(Value::as_array)
            .context("configOptions should be an array")?;
        assert_eq!(config_options.len(), 3);
        let model_option = config_options
            .iter()
            .find(|option| option.get("id").and_then(Value::as_str) == Some("model"))
            .context("model config option should be present")?;
        assert_eq!(
            model_option.get("currentValue").and_then(Value::as_str),
            Some("mock-flash")
        );
        assert_eq!(
            model_option
                .pointer("/options/0/value")
                .and_then(Value::as_str),
            Some("mock-model")
        );
        assert_eq!(
            model_option
                .pointer("/options/1/value")
                .and_then(Value::as_str),
            Some("mock-flash")
        );

        let _ = websocket.close(None).await;
        state.worker_pool.shutdown_all().await;
        let _ = timeout(Duration::from_secs(1), server_task).await;
        let _ = std::fs::remove_file(fake_worker_script);
        Ok(())
    }

    #[tokio::test]
    async fn client_response_is_routed_back_to_worker_request_id() -> Result<()> {
        let (fake_worker_exe, fake_worker_args, fake_worker_script) =
            fake_client_request_worker_command()?;
        let profile_id = "test-profile".to_string();
        let secret = "test-secret".to_string();
        let event_bus = Arc::new(SessionEventBus::new());
        let config_cache = Arc::new(SessionConfigCache::new());
        let permissions = Arc::new(SupervisorPermissionRegistry::new());
        let state = Arc::new(SupervisorState {
            config: SupervisorConfig {
                endpoint: SupervisorEndpointConfig {
                    websocket_bind_addr: "127.0.0.1:0".to_string(),
                    secret: secret.clone(),
                },
                profiles: vec![SupervisorProfileConfig {
                    id: profile_id.clone(),
                    path: PathBuf::from("unused-profile-path"),
                }],
                pool: SupervisorPoolConfig {
                    max_workers: 1,
                    idle_seconds: 60,
                },
                ..SupervisorConfig::default()
            },
            worker_pool: WorkerPool::new_with_launcher(
                WorkerLauncher::custom(fake_worker_exe, fake_worker_args, Vec::new()),
                event_bus.clone(),
                config_cache.clone(),
                permissions.clone(),
            ),
            event_bus,
            config_cache,
            permissions,
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_state = state.clone();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            handle_supervisor_connection(stream, server_state).await
        });

        let stream = tokio::net::TcpStream::connect(addr).await?;
        let url = format!("ws://{addr}");
        let (mut websocket, _) = tokio_tungstenite::client_async(url, stream).await?;
        assert!(websocket.next().await.context("missing ready")??.is_text());

        websocket
            .send(Message::Text(
                json!({
                    "id": "prompt",
                    "type": "worker.request",
                    "secret": secret,
                    "profile": profile_id,
                    "method": "session/prompt",
                    "params": { "sessionId": "s1" },
                })
                .to_string(),
            ))
            .await?;

        let client_request = wait_supervisor_client_request(&mut websocket).await?;
        assert_eq!(
            client_request.get("method").and_then(Value::as_str),
            Some("session/request_permission")
        );
        let routed_id = client_request
            .get("id")
            .cloned()
            .context("supervisor client request should include id")?;
        assert!(
            routed_id
                .as_str()
                .is_some_and(|id| id.starts_with("_dwo_worker_client_request:"))
        );

        websocket
            .send(Message::Text(
                json!({
                    "id": "client-response",
                    "type": "client.response",
                    "secret": "test-secret",
                    "message": {
                        "jsonrpc": "2.0",
                        "id": routed_id,
                        "result": { "decision": "allow_once" }
                    },
                })
                .to_string(),
            ))
            .await?;

        let prompt = wait_supervisor_result(&mut websocket, "prompt").await?;
        assert_eq!(
            prompt
                .pointer("/result/result/stopReason")
                .and_then(Value::as_str),
            Some("end_turn")
        );

        let _ = websocket.close(None).await;
        state.worker_pool.shutdown_all().await;
        let _ = timeout(Duration::from_secs(1), server_task).await;
        let _ = std::fs::remove_file(fake_worker_script);
        Ok(())
    }

    async fn wait_supervisor_result(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        id: &str,
    ) -> Result<Value> {
        timeout(Duration::from_secs(3), async {
            loop {
                let message = websocket
                    .next()
                    .await
                    .context("websocket closed before supervisor result")??;
                if !message.is_text() {
                    continue;
                }
                let value: Value = serde_json::from_str(message.to_text()?)?;
                if value.get("type").and_then(Value::as_str) == Some("supervisor.result")
                    && value.get("id").and_then(Value::as_str) == Some(id)
                {
                    return Ok(value);
                }
            }
        })
        .await?
    }

    async fn wait_supervisor_event(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        method: &str,
    ) -> Result<Value> {
        timeout(Duration::from_secs(3), async {
            loop {
                let message = websocket
                    .next()
                    .await
                    .context("websocket closed before supervisor event")??;
                if !message.is_text() {
                    continue;
                }
                let value: Value = serde_json::from_str(message.to_text()?)?;
                if value.get("type").and_then(Value::as_str) == Some("supervisor.event")
                    && value.pointer("/event/method").and_then(Value::as_str) == Some(method)
                {
                    return Ok(value);
                }
            }
        })
        .await?
    }

    async fn wait_supervisor_client_request(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> Result<Value> {
        timeout(Duration::from_secs(3), async {
            loop {
                let message = websocket
                    .next()
                    .await
                    .context("websocket closed before supervisor client request")??;
                if !message.is_text() {
                    continue;
                }
                let value: Value = serde_json::from_str(message.to_text()?)?;
                if value.get("type").and_then(Value::as_str) == Some("supervisor.client_request") {
                    return Ok(value);
                }
            }
        })
        .await?
    }

    fn fake_worker_command() -> Result<(PathBuf, Vec<OsString>, PathBuf)> {
        let script_path =
            std::env::temp_dir().join(format!("dwo-supervisor-fake-worker-{}.ps1", Uuid::new_v4()));
        std::fs::write(
            &script_path,
            r#"
$pending = $null
while ($null -ne ($line = [Console]::In.ReadLine())) {
  try {
    $message = $line | ConvertFrom-Json
  } catch {
    continue
  }

  if ($null -ne $message.id) {
    $pending = [string]$message.id
    continue
  }

  if ($message.method -eq 'session/cancel') {
    $response = @{
      jsonrpc = '2.0'
      id = $pending
      result = @{ stopReason = 'cancelled' }
    } | ConvertTo-Json -Compress
    [Console]::Out.WriteLine($response)
    [Console]::Out.Flush()
    exit 0
  }
}
"#,
        )
        .with_context(|| format!("write fake worker script {}", script_path.display()))?;
        let exe = find_executable("pwsh.exe")
            .or_else(|| find_executable("powershell.exe"))
            .context("PowerShell is required for supervisor fake worker test")?;
        Ok((
            exe,
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                script_path.clone().into_os_string(),
            ],
            script_path,
        ))
    }

    fn fake_event_worker_command() -> Result<(PathBuf, Vec<OsString>, PathBuf)> {
        let script_path = std::env::temp_dir().join(format!(
            "dwo-supervisor-fake-event-worker-{}.ps1",
            Uuid::new_v4()
        ));
        std::fs::write(
            &script_path,
            r#"
$pending = $null
while ($null -ne ($line = [Console]::In.ReadLine())) {
  try {
    $message = $line | ConvertFrom-Json
  } catch {
    continue
  }

  if ($null -eq $message.id) {
    continue
  }

  if ($message.method -eq '_dwo/ingress/handle_event') {
    $event = @{
      jsonrpc = '2.0'
      method = 'session/update'
      params = @{
        sessionId = 's1'
        update = @{
          sessionUpdate = 'agent_message_chunk'
          content = @{ type = 'text'; text = 'remote' }
        }
      }
    } | ConvertTo-Json -Depth 12 -Compress
    [Console]::Out.WriteLine($event)
  }

  $response = @{
    jsonrpc = '2.0'
    id = [string]$message.id
    result = @{}
  }
  if ($message.method -eq '_dwo/ingress/handle_event') {
    $response.result = @{ actions = @() }
  }
  [Console]::Out.WriteLine(($response | ConvertTo-Json -Depth 12 -Compress))
  [Console]::Out.Flush()
}
"#,
        )
        .with_context(|| format!("write fake worker script {}", script_path.display()))?;
        fake_worker_process(script_path)
    }

    fn fake_concurrent_worker_command() -> Result<(PathBuf, Vec<OsString>, PathBuf)> {
        let script_path = std::env::temp_dir().join(format!(
            "dwo-supervisor-fake-concurrent-worker-{}.ps1",
            Uuid::new_v4()
        ));
        std::fs::write(
            &script_path,
            r#"
$pendingPrompt = $null
while ($null -ne ($line = [Console]::In.ReadLine())) {
  try {
    $message = $line | ConvertFrom-Json
  } catch {
    continue
  }

  if ($null -eq $message.id) {
    continue
  }

  $id = [string]$message.id
  $method = [string]$message.method
  $sessionId = [string]$message.params.sessionId

  if ($method -eq 'session/prompt' -and $sessionId -eq 's1') {
    $pendingPrompt = $id
    continue
  }

  if ($method -eq 'session/load' -or $method -eq '_dwo/session/load') {
    $result = @{ sessionId = $sessionId }
    if ($method -eq '_dwo/session/load') {
      $result = @{
        response = @{ sessionId = $sessionId }
        replayEvents = @()
      }
    }
    $response = @{
      jsonrpc = '2.0'
      id = $id
      result = $result
    } | ConvertTo-Json -Depth 12 -Compress
    [Console]::Out.WriteLine($response)
    [Console]::Out.Flush()
    continue
  }

  if ($method -eq 'session/prompt') {
    $response = @{
      jsonrpc = '2.0'
      id = $id
      result = @{ sessionId = $sessionId; stopReason = 'end_turn' }
    } | ConvertTo-Json -Depth 12 -Compress
    [Console]::Out.WriteLine($response)
    [Console]::Out.Flush()
    continue
  }

  if ($method -eq '_dwo/worker/shutdown') {
    $response = @{
      jsonrpc = '2.0'
      id = $id
      result = @{ ok = $true }
    } | ConvertTo-Json -Depth 12 -Compress
    [Console]::Out.WriteLine($response)
    [Console]::Out.Flush()
    exit 0
  }

  $response = @{
    jsonrpc = '2.0'
    id = $id
    result = @{}
  } | ConvertTo-Json -Depth 12 -Compress
  [Console]::Out.WriteLine($response)
  [Console]::Out.Flush()
}
"#,
        )
        .with_context(|| format!("write fake worker script {}", script_path.display()))?;
        fake_worker_process(script_path)
    }

    fn fake_client_request_worker_command() -> Result<(PathBuf, Vec<OsString>, PathBuf)> {
        let script_path = std::env::temp_dir().join(format!(
            "dwo-supervisor-fake-client-request-worker-{}.ps1",
            Uuid::new_v4()
        ));
        std::fs::write(
            &script_path,
            r#"
$promptId = $null
$workerRequestId = 'permission-1'
while ($null -ne ($line = [Console]::In.ReadLine())) {
  try {
    $message = $line | ConvertFrom-Json
  } catch {
    continue
  }

  if ($null -ne $message.id -and $message.method -eq 'session/prompt') {
    $promptId = [string]$message.id
    $request = @{
      jsonrpc = '2.0'
      id = $workerRequestId
      method = 'session/request_permission'
      params = @{
        sessionId = 's1'
        toolCallId = 'call-1'
      }
    } | ConvertTo-Json -Depth 12 -Compress
    [Console]::Out.WriteLine($request)
    [Console]::Out.Flush()
    continue
  }

  if ($null -ne $message.id -and $null -eq $message.method) {
    if ([string]$message.id -ne $workerRequestId) {
      $response = @{
        jsonrpc = '2.0'
        id = $promptId
        error = @{ code = -32000; message = "unexpected client response id: $($message.id)" }
      } | ConvertTo-Json -Depth 12 -Compress
      [Console]::Out.WriteLine($response)
      [Console]::Out.Flush()
      continue
    }
    $response = @{
      jsonrpc = '2.0'
      id = $promptId
      result = @{ stopReason = 'end_turn' }
    } | ConvertTo-Json -Depth 12 -Compress
    [Console]::Out.WriteLine($response)
    [Console]::Out.Flush()
    continue
  }

  if ($null -ne $message.id -and $message.method -eq '_dwo/worker/shutdown') {
    $response = @{
      jsonrpc = '2.0'
      id = [string]$message.id
      result = @{ ok = $true }
    } | ConvertTo-Json -Depth 12 -Compress
    [Console]::Out.WriteLine($response)
    [Console]::Out.Flush()
    exit 0
  }
}
"#,
        )
        .with_context(|| format!("write fake worker script {}", script_path.display()))?;
        fake_worker_process(script_path)
    }

    fn fake_control_worker_command() -> Result<(PathBuf, Vec<OsString>, PathBuf)> {
        let script_path = std::env::temp_dir().join(format!(
            "dwo-supervisor-fake-control-worker-{}.ps1",
            Uuid::new_v4()
        ));
        std::fs::write(
            &script_path,
            r#"
while ($null -ne ($line = [Console]::In.ReadLine())) {
  try {
    $message = $line | ConvertFrom-Json
  } catch {
    continue
  }

  if ($null -ne $message.id) {
    if ($message.method -eq 'session/prompt') {
      $pending = [string]$message.id
      continue
    }
    if ($message.method -eq 'session/new') {
      $response = @{
        jsonrpc = '2.0'
        id = [string]$message.id
        result = @{
          sessionId = 's1'
          configOptions = @(
            @{
              id = 'policy_mode'
              name = 'Policy'
              category = 'mode'
              type = 'select'
              currentValue = 'full_access'
              options = @(
                @{ value = 'full_access'; name = 'Full Access' },
                @{ value = 'confirm'; name = 'Confirm' },
                @{ value = 'watch'; name = 'Watch' }
              )
            },
            @{
              id = 'model'
              name = 'Model'
              category = 'model'
              type = 'select'
              currentValue = 'mock-model'
              options = @(
                @{ value = 'mock-model'; name = 'Mock Model' },
                @{ value = 'mock-flash'; name = 'Mock Flash' }
              )
            },
            @{
              id = 'reasoning_mode'
              name = 'Reasoning Mode'
              category = 'thought_level'
              type = 'select'
              currentValue = 'auto'
              options = @(
                @{ value = 'auto'; name = 'auto' },
                @{ value = 'max'; name = 'max' }
              )
            }
          )
        }
      } | ConvertTo-Json -Depth 12 -Compress
      [Console]::Out.WriteLine($response)
      [Console]::Out.Flush()
      continue
    }
    if ($message.method -eq 'session/set_config_option') {
      $configId = [string]$message.params.configId
      $value = [string]$message.params.value
      $response = @{
        jsonrpc = '2.0'
        id = [string]$message.id
        result = @{
          configOptions = @(
            @{
              id = $configId
              name = $configId
              category = 'model'
              type = 'select'
              currentValue = $value
              options = @(
                @{ value = 'mock-model'; name = 'Mock Model' },
                @{ value = 'mock-flash'; name = 'Mock Flash' }
              )
            }
          )
        }
      } | ConvertTo-Json -Depth 12 -Compress
      [Console]::Out.WriteLine($response)
      [Console]::Out.Flush()
      continue
    }
    $response = @{
      jsonrpc = '2.0'
      id = [string]$message.id
      result = @{}
    } | ConvertTo-Json -Depth 12 -Compress
    [Console]::Out.WriteLine($response)
    [Console]::Out.Flush()
  } elseif ($message.method -eq '_dwo/session/set_config_option') {
    if ($null -ne $pending) {
      $response = @{
        jsonrpc = '2.0'
        id = $pending
        result = @{ stopReason = 'cancelled' }
      } | ConvertTo-Json -Depth 12 -Compress
      [Console]::Out.WriteLine($response)
      [Console]::Out.Flush()
      $pending = $null
    }
  }
}
"#,
        )
        .with_context(|| format!("write fake worker script {}", script_path.display()))?;
        fake_worker_process(script_path)
    }

    fn fake_worker_process(script_path: PathBuf) -> Result<(PathBuf, Vec<OsString>, PathBuf)> {
        let exe = find_executable("pwsh.exe")
            .or_else(|| find_executable("powershell.exe"))
            .context("PowerShell is required for supervisor fake worker test")?;
        Ok((
            exe,
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                script_path.clone().into_os_string(),
            ],
            script_path,
        ))
    }
}
