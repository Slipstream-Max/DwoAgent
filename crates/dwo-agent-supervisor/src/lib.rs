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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioCommand};
use tokio::sync::Mutex;
use uuid::Uuid;

const WINDOWS_TASK_NAME: &str = "DwoAgentSupervisor";
const WINDOWS_LAUNCHER_FILE: &str = "supervisor-startup.vbs";
const MACOS_LAUNCH_AGENT_ID: &str = "com.dwoagent.supervisor";
const LINUX_SYSTEMD_UNIT: &str = "dwoagent-supervisor.service";
const LINUX_DESKTOP_FILE: &str = "dwoagent-supervisor.desktop";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

    let suppressed_ids = Arc::new(Mutex::new(HashSet::<String>::new()));
    let suppressed_for_stdin = suppressed_ids.clone();
    let secret = config.endpoint.secret.clone();
    let stdin_profile = profile_id.clone();
    let stdin_task = tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        while let Some(line) = stdin.next_line().await.context("read ACP stdin")? {
            let message: Value = serde_json::from_str(&line).context("parse ACP JSON-RPC")?;
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
            write
                .send(Message::Text(supervisor_message.to_string()))
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
                "supervisor.result" | "supervisor.error" => {
                    let id = value.get("id").cloned().unwrap_or(Value::Null);
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
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32000,
                                "message": value
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("supervisor request failed"),
                            },
                        })
                    };
                    write_stdout_json(&stdout_for_ws, out).await?;
                }
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    tokio::select! {
        result = stdin_task => result.context("ACP stdin task panicked")?,
        result = ws_task => result.context("supervisor websocket task panicked")?,
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
                state.worker_pool.shutdown_all().await;
                return Ok(());
            },
        }
    }
}

struct SupervisorState {
    config: SupervisorConfig,
    worker_pool: WorkerPool,
}

impl SupervisorState {
    fn new(config: SupervisorConfig) -> Result<Self> {
        Ok(Self {
            worker_pool: WorkerPool::new(env::current_exe().context("resolve current executable")?),
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
                .context("no profile requested and supervisor.default_profile is not set")?,
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
        emit_event: F,
    ) -> SupervisorResponse
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let id = request.id.clone();
        match self.dispatch_with_events(request, emit_event).await {
            Ok(result) => SupervisorResponse::result(id, result),
            Err(err) => SupervisorResponse::error(id, err.to_string()),
        }
    }

    async fn dispatch_with_events<F, Fut>(
        &self,
        request: SupervisorRequest,
        mut emit_event: F,
    ) -> Result<Value>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.validate_secret(request.secret.as_deref())?;
        match request.kind.as_str() {
            "supervisor.ping" => Ok(json!({ "ok": true })),
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
                let request_id = request.id.clone();
                let profile_id = profile.id.clone();
                let result = self
                    .worker_pool
                    .request_with_events(
                        &profile,
                        method,
                        request.params.unwrap_or(Value::Null),
                        &self.config.pool,
                        |event| {
                            emit_event(json!({
                                "id": request_id.clone(),
                                "type": "supervisor.event",
                                "profile": profile_id.clone(),
                                "event": event,
                            }))
                        },
                    )
                    .await?;
                Ok(json!({
                    "profile": profile.id,
                    "result": result,
                }))
            }
            "worker.notify" => {
                let method = request
                    .method
                    .as_deref()
                    .context("worker.notify requires `method`")?;
                let profile = self.resolve_profile(request.profile.as_deref())?;
                self.worker_pool
                    .notify(
                        &profile,
                        method,
                        request.params.unwrap_or(Value::Null),
                        &self.config.pool,
                    )
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
    exe: PathBuf,
    workers: Mutex<HashMap<String, Arc<Mutex<WorkerProcess>>>>,
}

impl WorkerPool {
    fn new(exe: PathBuf) -> Self {
        Self {
            exe,
            workers: Mutex::new(HashMap::new()),
        }
    }

    async fn request_with_events<F, Fut>(
        &self,
        profile: &ResolvedProfile,
        method: &str,
        params: Value,
        pool_config: &SupervisorPoolConfig,
        emit_event: F,
    ) -> Result<Value>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let worker = self.ensure_worker(profile, pool_config).await?;
        let mut worker = worker.lock().await;
        worker.last_used = Instant::now();
        worker.request_with_events(method, params, emit_event).await
    }

    async fn notify(
        &self,
        profile: &ResolvedProfile,
        method: &str,
        params: Value,
        pool_config: &SupervisorPoolConfig,
    ) -> Result<()> {
        let worker = self.ensure_worker(profile, pool_config).await?;
        let mut worker = worker.lock().await;
        worker.last_used = Instant::now();
        worker.notify(method, params).await
    }

    async fn ensure_worker(
        &self,
        profile: &ResolvedProfile,
        pool_config: &SupervisorPoolConfig,
    ) -> Result<Arc<Mutex<WorkerProcess>>> {
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

        let worker = Arc::new(Mutex::new(
            WorkerProcess::spawn(&self.exe, profile)
                .await
                .with_context(|| format!("spawn worker for profile `{}`", profile.id))?,
        ));
        workers.insert(profile.id.clone(), worker.clone());
        Ok(worker)
    }

    async fn prune_idle(&self, pool_config: &SupervisorPoolConfig) {
        let idle_after = Duration::from_secs(pool_config.idle_seconds.max(1));
        let now = Instant::now();
        let mut workers = self.workers.lock().await;
        let stale = {
            let mut stale = Vec::new();
            for (id, worker) in workers.iter() {
                let worker = worker.lock().await;
                if now.duration_since(worker.last_used) >= idle_after {
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
            let worker = worker.lock().await;
            items.push(json!({
                "profile": profile,
                "pid": worker.child.id(),
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

async fn oldest_worker_key(
    workers: &HashMap<String, Arc<Mutex<WorkerProcess>>>,
    exclude: Option<&str>,
) -> Option<String> {
    let mut oldest: Option<(String, Instant)> = None;
    for (id, worker) in workers {
        if exclude == Some(id.as_str()) {
            continue;
        }
        let worker = worker.lock().await;
        if oldest
            .as_ref()
            .is_none_or(|(_, last_used)| worker.last_used < *last_used)
        {
            oldest = Some((id.clone(), worker.last_used));
        }
    }
    oldest.map(|(id, _)| id)
}

async fn stop_worker(worker: Arc<Mutex<WorkerProcess>>) -> Result<()> {
    let mut worker = worker.lock().await;
    let _ = worker.request("_dwo/worker/shutdown", json!({})).await;
    if worker.child.try_wait()?.is_none() {
        let _ = worker.child.kill().await;
    }
    Ok(())
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    last_used: Instant,
}

impl WorkerProcess {
    async fn spawn(exe: &Path, profile: &ResolvedProfile) -> Result<Self> {
        let mut child = TokioCommand::new(exe)
            .arg("agent")
            .arg("run")
            .arg("--agent-profile")
            .arg(&profile.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn {}", exe.display()))?;
        let stdin = child.stdin.take().context("worker stdin was not piped")?;
        let stdout = child.stdout.take().context("worker stdout was not piped")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            last_used: Instant::now(),
        })
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_with_events(method, params, |_| async { Ok(()) })
            .await
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_vec(&request).context("serialize worker notification")?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .context("write worker notification")?;
        self.stdin
            .flush()
            .await
            .context("flush worker notification")?;
        Ok(())
    }

    async fn request_with_events<F, Fut>(
        &mut self,
        method: &str,
        params: Value,
        mut emit_event: F,
    ) -> Result<Value>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let id = Uuid::new_v4().to_string();
        let request = WorkerRequest {
            jsonrpc: "2.0",
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        let mut line = serde_json::to_vec(&request).context("serialize worker request")?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .context("write worker request")?;
        self.stdin.flush().await.context("flush worker request")?;

        loop {
            let Some(response_line) = self
                .stdout
                .next_line()
                .await
                .context("read worker response")?
            else {
                bail!("worker exited before responding");
            };
            let response: WorkerMessage =
                serde_json::from_str(&response_line).context("parse worker response")?;
            if let Some(method) = response.method {
                emit_event(json!({
                    "method": method,
                    "params": response.params.unwrap_or(Value::Null),
                }))
                .await?;
                continue;
            }
            if response.id.as_ref() != Some(&Value::String(id.clone())) {
                bail!("worker response id mismatch");
            }
            if let Some(error) = response.error {
                bail!("{}", worker_error_message(&error));
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }
}

fn worker_error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| error.to_string())
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
    let write = Arc::new(Mutex::new(write));
    write
        .lock()
        .await
        .send(Message::Text(
            json!({
                "type": "supervisor.ready",
                "protocol": "dwo-supervisor-v1",
            })
            .to_string(),
        ))
        .await
        .context("send supervisor hello")?;
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
        let response = match serde_json::from_str::<SupervisorRequest>(&text) {
            Ok(request) => {
                let write_for_events = write.clone();
                state
                    .handle_request_with_events(request, move |event| {
                        let write = write_for_events.clone();
                        async move {
                            write
                                .lock()
                                .await
                                .send(Message::Text(serde_json::to_string(&event)?))
                                .await
                                .context("send supervisor event")
                        }
                    })
                    .await
            }
            Err(err) => SupervisorResponse::error(None, format!("invalid request: {err}")),
        };
        write
            .lock()
            .await
            .send(Message::Text(serde_json::to_string(&response)?))
            .await
            .context("send supervisor response")?;
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
      If InStr(1, command, "supervisor run", 1) > 0 Then
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
    let script = "$matches = foreach ($process in Get-CimInstance Win32_Process) { \
         $command = $process.CommandLine; \
         if ($null -ne $command -and $command.IndexOf('supervisor run', [StringComparison]::OrdinalIgnoreCase) -ge 0) { \
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
