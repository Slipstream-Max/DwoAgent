use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use dwo_agent_service::{
    EndpointId, SessionConfigUpdate, SessionEventPayload, SessionId, TranscriptItem,
};
use dwo_tools::ConfirmationDecision;
use tokio::sync::Mutex;
use weixin_agent::{MessageContext, MessageHandler, WeixinClient, WeixinConfig};

use crate::host::Host;

use super::manager::{ChannelState, StreamMode, WEIXIN_FLUSH_INTERVAL_MS};

pub struct GatewayHub {
    active: Mutex<Option<ActiveChannel>>,
}

struct ActiveChannel {
    client: Arc<WeixinClient>,
    client_task: tokio::task::JoinHandle<()>,
    observer: Arc<Mutex<Option<WeixinObserver>>>,
}

impl GatewayHub {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
        }
    }

    pub async fn start_all(self: &Arc<Self>, host: Arc<Host>) {
        let channels = match host.channels.list().await {
            Ok(channels) => channels,
            Err(error) => {
                eprintln!("load channels: {error:#}");
                return;
            }
        };
        let should_start = channels
            .into_iter()
            .any(|channel| channel.name == "weixin" && channel.enabled && channel.connected);
        if should_start && let Err(error) = self.start_weixin(host).await {
            eprintln!("start Weixin channel: {error:#}");
        }
    }

    pub async fn start_weixin(self: &Arc<Self>, host: Arc<Host>) -> Result<()> {
        if self.active.lock().await.is_some() {
            return Ok(());
        }
        let runtime = host.channels.load_weixin().await?;
        let state = Arc::new(Mutex::new(runtime.state));
        let client_ref = Arc::new(OnceLock::new());
        let observer = Arc::new(Mutex::new(None));
        let handler = WeixinHandler {
            host: host.clone(),
            bound_user_id: runtime.secret.bound_user_id,
            default_stream_mode: runtime.config.stream_mode,
            replay_turns: runtime.config.replay_turns,
            state: state.clone(),
            client: client_ref.clone(),
            observer: observer.clone(),
        };
        let config = WeixinConfig::builder()
            .token(runtime.secret.bot_token)
            .base_url(runtime.secret.base_url)
            .markdown_filter(runtime.config.markdown_filter)
            .build()?;
        let startup_handler = handler.clone();
        let client = Arc::new(WeixinClient::builder(config).on_message(handler).build()?);
        client
            .context_tokens()
            .import(state.lock().await.context_tokens.clone());
        let _ = client_ref.set(client.clone());
        if let Err(error) = startup_handler.resume_observer().await {
            eprintln!("restore Weixin session observer: {error:#}");
        }
        let running_client = client.clone();
        let task = tokio::spawn(async move {
            let sync_buf = state.lock().await.sync_buf.clone();
            if let Err(error) = running_client.start(sync_buf).await {
                eprintln!("Weixin channel stopped: {error}");
            }
        });
        *self.active.lock().await = Some(ActiveChannel {
            client,
            client_task: task,
            observer,
        });
        Ok(())
    }

    pub async fn stop(&self) {
        let active = self.active.lock().await.take();
        if let Some(active) = active {
            active.client.shutdown();
            if let Some(observer) = active.observer.lock().await.take() {
                observer.task.abort();
            }
            let mut client_task = active.client_task;
            if tokio::time::timeout(Duration::from_secs(5), &mut client_task)
                .await
                .is_err()
            {
                client_task.abort();
            }
        }
    }

    pub async fn send_weixin_message(&self, to: &str, text: &str) -> Result<()> {
        let client = self.weixin_client().await?;
        let context_token = client.context_tokens().get(to);
        client.send_text(to, text, context_token.as_deref()).await?;
        Ok(())
    }

    pub async fn send_weixin_file(&self, to: &str, path: &std::path::Path) -> Result<()> {
        if !path.is_file() {
            bail!("file does not exist: {}", path.display());
        }
        let client = self.weixin_client().await?;
        let context_token = client.context_tokens().get(to);
        client
            .send_media(to, path, context_token.as_deref())
            .await?;
        Ok(())
    }

    async fn weixin_client(&self) -> Result<Arc<WeixinClient>> {
        self.active
            .lock()
            .await
            .as_ref()
            .map(|active| active.client.clone())
            .context("Weixin channel is not running")
    }
}

#[derive(Clone)]
struct WeixinHandler {
    host: Arc<Host>,
    bound_user_id: String,
    default_stream_mode: StreamMode,
    replay_turns: usize,
    state: Arc<Mutex<ChannelState>>,
    client: Arc<OnceLock<Arc<WeixinClient>>>,
    observer: Arc<Mutex<Option<WeixinObserver>>>,
}

struct WeixinObserver {
    session_id: String,
    task: tokio::task::JoinHandle<()>,
}

#[async_trait]
impl MessageHandler for WeixinHandler {
    async fn on_message(&self, ctx: &MessageContext) -> weixin_agent::Result<()> {
        if ctx.from != self.bound_user_id {
            return Ok(());
        }
        let text = ctx.body.as_deref().unwrap_or("").trim();
        let result = if text.starts_with('/') {
            self.handle_command(ctx, text).await
        } else if text.is_empty() {
            Ok(())
        } else {
            self.handle_prompt(ctx, text).await
        };
        if let Err(error) = result {
            let _ = ctx.reply_text(&format!("dwoagent error: {error:#}")).await;
        }
        Ok(())
    }

    async fn on_sync_buf_updated(&self, sync_buf: &str) -> weixin_agent::Result<()> {
        if let Err(error) = self.save_runtime(Some(sync_buf)).await {
            eprintln!("save Weixin sync state: {error:#}");
        }
        Ok(())
    }

    async fn on_shutdown(&self) -> weixin_agent::Result<()> {
        if let Err(error) = self.save_runtime(None).await {
            eprintln!("save Weixin shutdown state: {error:#}");
        }
        Ok(())
    }
}

impl WeixinHandler {
    async fn save_runtime(&self, sync_buf: Option<&str>) -> Result<()> {
        let snapshot = {
            let mut state = self.state.lock().await;
            if let Some(sync_buf) = sync_buf {
                state.sync_buf = Some(sync_buf.to_string());
            }
            if let Some(client) = self.client.get() {
                state.context_tokens = client.context_tokens().export_all();
            }
            state.clone()
        };
        self.host.channels.save_state(&snapshot).await
    }

    async fn handle_command(&self, ctx: &MessageContext, text: &str) -> Result<()> {
        let mut parts = text.split_whitespace();
        let command = parts.next().unwrap_or("/help");
        match command {
            "/help" => {
                ctx.reply_text(
                    "/list /new [name] /use <session> /status /del <session> /cancel\n\
                     /model <name> /reasoning <level|off> /allow <id> /deny <id>\n\
                     /stream answer|full",
                )
                .await?;
            }
            "/list" => {
                let records = self.host.service.list().await?;
                let selected = self.state.lock().await.selected_session_id.clone();
                let text = records
                    .into_iter()
                    .map(|record| {
                        format!(
                            "{} {} {}",
                            if selected.as_deref() == Some(record.info.id.as_str()) {
                                "*"
                            } else {
                                "-"
                            },
                            record.info.id,
                            record.info.title
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ctx.reply_text(if text.is_empty() {
                    "No sessions"
                } else {
                    &text
                })
                .await?;
            }
            "/new" => {
                let title = parts.next().map(str::to_string);
                let session = self.host.create_session(title, None).await?;
                self.select_session(session.id().as_str()).await?;
                ctx.reply_text(&format!("Selected new session {}", session.id()))
                    .await?;
            }
            "/use" => {
                let id = parts.next().context("usage: /use <session>")?;
                let session_id = SessionId::parse(id.to_string()).map_err(anyhow::Error::msg)?;
                let agent = self.host.service.load(&session_id).await?;
                self.select_session(id).await?;
                let subscription = agent.attach(self.endpoint(&ctx.from)).await?;
                ctx.reply_text(&render_replay(&subscription.snapshot, self.replay_turns))
                    .await?;
            }
            "/status" => {
                let agent = self.selected_agent().await?;
                let snapshot = agent.attach(self.endpoint(&ctx.from)).await?.snapshot;
                ctx.reply_text(&render_replay(&snapshot, self.replay_turns))
                    .await?;
            }
            "/del" => {
                let id = parts.next().context("usage: /del <session>")?;
                let session_id = SessionId::parse(id.to_string()).map_err(anyhow::Error::msg)?;
                self.host.service.delete(&session_id).await?;
                let mut state = self.state.lock().await;
                if state.selected_session_id.as_deref() == Some(id) {
                    state.selected_session_id = None;
                    self.host.channels.save_state(&state).await?;
                }
                ctx.reply_text("Session deleted").await?;
            }
            "/cancel" => {
                self.selected_agent().await?.cancel(None).await?;
                ctx.reply_text("Cancellation requested").await?;
            }
            "/model" => {
                let model = parts.next().context("usage: /model <name>")?;
                let id = self.selected_session_id().await?;
                self.host
                    .service
                    .set_config(&id, SessionConfigUpdate::Model(model.to_string()))
                    .await?;
                ctx.reply_text("Model updated").await?;
            }
            "/reasoning" => {
                let value = parts.next().context("usage: /reasoning <level|off>")?;
                let reasoning = (value != "off").then(|| value.to_string());
                let id = self.selected_session_id().await?;
                self.host
                    .service
                    .set_config(&id, SessionConfigUpdate::Reasoning(reasoning))
                    .await?;
                ctx.reply_text("Reasoning updated").await?;
            }
            "/allow" | "/deny" => {
                let request_id = parts.next().context("permission id is required")?;
                let allowed = command == "/allow";
                self.selected_agent()
                    .await?
                    .respond_permission(
                        self.endpoint(&ctx.from),
                        request_id.to_string(),
                        ConfirmationDecision {
                            allowed,
                            reason: (!allowed).then(|| "denied from Weixin".to_string()),
                        },
                    )
                    .await?;
                ctx.reply_text("Permission resolved").await?;
            }
            "/stream" => {
                let mode = match parts.next() {
                    Some("answer") => StreamMode::Answer,
                    Some("full") => StreamMode::Full,
                    _ => bail!("usage: /stream answer|full"),
                };
                let mut state = self.state.lock().await;
                state.stream_mode = Some(mode);
                self.host.channels.save_state(&state).await?;
                ctx.reply_text("Stream mode updated").await?;
            }
            _ => bail!("unknown command: {command}"),
        }
        Ok(())
    }

    async fn handle_prompt(&self, ctx: &MessageContext, text: &str) -> Result<()> {
        let selected = self.state.lock().await.selected_session_id.clone();
        let agent = if let Some(id) = selected {
            let session_id = SessionId::parse(id).map_err(anyhow::Error::msg)?;
            self.host.service.load(&session_id).await?
        } else {
            let agent = self
                .host
                .create_session(Some("weixin".to_string()), None)
                .await?;
            self.select_session(agent.id().as_str()).await?;
            agent
        };
        let endpoint = self.endpoint(&ctx.from);
        self.ensure_observer(agent.clone()).await?;
        agent.prompt(endpoint, text.to_string()).await?;
        Ok(())
    }

    async fn selected_session_id(&self) -> Result<SessionId> {
        let id = self
            .state
            .lock()
            .await
            .selected_session_id
            .clone()
            .context("No session selected. Use /new or /use")?;
        SessionId::parse(id).map_err(anyhow::Error::msg)
    }

    async fn selected_agent(&self) -> Result<Arc<dwo_agent_service::SessionAgent>> {
        Ok(self
            .host
            .service
            .load(&self.selected_session_id().await?)
            .await?)
    }

    async fn select_session(&self, id: &str) -> Result<()> {
        {
            let mut state = self.state.lock().await;
            state.selected_session_id = Some(id.to_string());
            self.host.channels.save_state(&state).await?;
        }
        let session_id = SessionId::parse(id.to_string()).map_err(anyhow::Error::msg)?;
        let agent = self.host.service.load(&session_id).await?;
        self.ensure_observer(agent).await
    }

    async fn resume_observer(&self) -> Result<()> {
        let Some(id) = self.state.lock().await.selected_session_id.clone() else {
            return Ok(());
        };
        let session_id = SessionId::parse(id).map_err(anyhow::Error::msg)?;
        let agent = self.host.service.load(&session_id).await?;
        self.ensure_observer(agent).await
    }

    async fn ensure_observer(&self, agent: Arc<dwo_agent_service::SessionAgent>) -> Result<()> {
        let session_id = agent.id().to_string();
        let mut observer = self.observer.lock().await;
        if observer
            .as_ref()
            .is_some_and(|current| current.session_id == session_id && !current.task.is_finished())
        {
            return Ok(());
        }
        if let Some(current) = observer.take() {
            current.task.abort();
        }
        let subscription = agent.attach(self.endpoint(&self.bound_user_id)).await?;
        let client = self
            .client
            .get()
            .cloned()
            .context("Weixin client is not ready")?;
        let target = self.bound_user_id.clone();
        let state = self.state.clone();
        let default_mode = self.default_stream_mode;
        let flush_interval = Duration::from_millis(WEIXIN_FLUSH_INTERVAL_MS);
        let task = tokio::spawn(stream_session(
            client,
            target,
            subscription,
            state,
            default_mode,
            flush_interval,
        ));
        *observer = Some(WeixinObserver { session_id, task });
        Ok(())
    }

    fn endpoint(&self, user: &str) -> EndpointId {
        let safe = format!("weixin-{user}")
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        EndpointId::parse(safe).expect("sanitized Weixin endpoint")
    }
}

async fn stream_session(
    client: Arc<WeixinClient>,
    target: String,
    mut subscription: dwo_agent_service::SessionSubscription,
    state: Arc<Mutex<ChannelState>>,
    default_mode: StreamMode,
    flush_interval: Duration,
) {
    let mut buffer = String::new();
    let mut ticker = tokio::time::interval(flush_interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                flush(&client, &target, &mut buffer).await;
            }
            event = subscription.events.recv() => {
                let Some(event) = event else { break };
                let mode = state.lock().await.stream_mode.unwrap_or(default_mode);
                match event.payload {
                    SessionEventPayload::UserPromptSubmitted { content, .. } => {
                        send(&client, &target, &format!("User: {content}")).await;
                    }
                    SessionEventPayload::AssistantDelta { delta, .. } => buffer.push_str(&delta),
                    SessionEventPayload::AssistantCompleted { .. } => flush(&client, &target, &mut buffer).await,
                    SessionEventPayload::ToolStarted { call, .. } if matches!(mode, StreamMode::Full) => {
                        send(&client, &target, &format!("Tool started: {}", call.tool_name)).await;
                    }
                    SessionEventPayload::ToolCompleted { result, .. } if matches!(mode, StreamMode::Full) => {
                        let status = result.output.get("status").and_then(serde_json::Value::as_str).unwrap_or("completed");
                        send(&client, &target, &format!("Tool {}: {status}", result.tool_name)).await;
                    }
                    SessionEventPayload::PermissionRequested { permission, .. } => {
                        send(&client, &target, &format!("Confirm {}\n/allow {}\n/deny {}", permission.tool_name, permission.request_id, permission.request_id)).await;
                    }
                    SessionEventPayload::TurnCompleted { .. } => flush(&client, &target, &mut buffer).await,
                    SessionEventPayload::TurnCancelled { .. } => send(&client, &target, "Turn cancelled").await,
                    SessionEventPayload::TurnFailed { error, .. } => send(&client, &target, &format!("Turn failed: {error}")).await,
                    _ => {}
                }
            }
        }
    }
}

async fn flush(client: &WeixinClient, target: &str, buffer: &mut String) {
    if buffer.is_empty() {
        return;
    }
    let text = std::mem::take(buffer);
    send(client, target, &text).await;
}

async fn send(client: &WeixinClient, target: &str, text: &str) {
    let context_token = client.context_tokens().get(target);
    if let Err(error) = client
        .send_text(target, text, context_token.as_deref())
        .await
    {
        eprintln!("send Weixin message: {error}");
    }
}

fn render_replay(snapshot: &dwo_agent_service::SessionSnapshot, turns: usize) -> String {
    let mut lines = vec![format!(
        "Session: {}\nModel: {}\nReasoning: {}\nState: {:?}",
        snapshot.record.info.title,
        snapshot.record.llm.model,
        snapshot
            .record
            .llm
            .reasoning
            .as_deref()
            .unwrap_or("default"),
        snapshot.phase,
    )];
    let transcript = &snapshot.record.context.transcript;
    let start = transcript.len().saturating_sub(turns.saturating_mul(2));
    for item in &transcript[start..] {
        match item {
            TranscriptItem::User { content, .. } => lines.push(format!("You: {content}")),
            TranscriptItem::Assistant { content, .. } => {
                lines.push(format!("Assistant: {content}"))
            }
            TranscriptItem::Tool { result, .. } => {
                let status = result
                    .output
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("completed");
                lines.push(format!("Tool {}: {status}", result.tool_name));
            }
        }
    }
    if !snapshot.partial_message.is_empty() {
        lines.push(format!("Current: {}", snapshot.partial_message));
    }
    if let Some(permission) = &snapshot.pending_permission {
        lines.push(format!("Pending permission: {}", permission.request_id));
    }
    lines.join("\n")
}
