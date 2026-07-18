use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{Datelike, Local};
use dwo_agent_service::{
    ClientTranscriptEvent, ContentBlock, EndpointId, MessageContent, SessionConfigUpdate,
    SessionEventPayload, SessionId,
};
use dwo_tools::{ConfirmationDecision, SessionMode};
use tokio::sync::Mutex;
use weixin_agent::{
    MediaInfo, MediaType, MessageContext, MessageHandler, WeixinClient, WeixinConfig,
};

use crate::host::Host;

use super::manager::ChannelState;

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
            replay_turns: runtime.config.replay_turns,
            media_input: runtime.config.media_input,
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
    replay_turns: usize,
    media_input: bool,
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
        } else if text.is_empty() && ctx.media.is_none() {
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
        let tokens = split_command_line(text)?;
        let command = tokens.first().map(String::as_str).unwrap_or("/help");
        let mut args = tokens.iter().skip(1).map(String::as_str);
        match command {
            "/help" => {
                ctx.reply_text(
                    "/list /new [name] [--cwd <path>] /use <session> /status /del <session> /cancel\n\
                     /model <name> /reasoning <level|off> /policy [full_access|confirm|watch]\n\
                     /allow <id> /deny <id>",
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
                let (title, cwd) = parse_new_args(&tokens[1..])?;
                let session = self.host.create_session(title, cwd).await?;
                self.select_session(session.id().as_str()).await?;
                let snapshot = session.attach(self.endpoint(&ctx.from)).await?.snapshot;
                ctx.reply_text(&format!(
                    "Selected new session {}\nCwd: {}",
                    session.id(),
                    display_path(&snapshot.record.info.cwd)
                ))
                .await?;
            }
            "/use" => {
                let id = args.next().context("usage: /use <session>")?;
                let session_id = SessionId::parse(id.to_string()).map_err(anyhow::Error::msg)?;
                let agent = self.host.service.load(&session_id).await?;
                self.select_session(id).await?;
                let subscription = agent.attach(self.endpoint(&ctx.from)).await?;
                let replay =
                    render_replay_turns(&subscription.snapshot.transcript, self.replay_turns);
                if replay.is_empty() {
                    ctx.reply_text(&render_status(&subscription.snapshot))
                        .await?;
                } else {
                    for turn in replay {
                        reply_logical_message(ctx, &turn).await?;
                    }
                }
            }
            "/status" => {
                let agent = self.selected_agent().await?;
                let snapshot = agent.attach(self.endpoint(&ctx.from)).await?.snapshot;
                ctx.reply_text(&render_status(&snapshot)).await?;
            }
            "/del" => {
                let id = args.next().context("usage: /del <session>")?;
                let session_id = SessionId::parse(id.to_string()).map_err(anyhow::Error::msg)?;
                self.host.delete_session(&session_id).await?;
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
                let model = args.next().context("usage: /model <name>")?;
                let id = self.selected_session_id().await?;
                self.host
                    .service
                    .set_config(&id, SessionConfigUpdate::Model(model.to_string()))
                    .await?;
                ctx.reply_text("Model updated").await?;
            }
            "/reasoning" => {
                let value = args.next().context("usage: /reasoning <level|off>")?;
                let reasoning = (value != "off").then(|| value.to_string());
                let id = self.selected_session_id().await?;
                self.host
                    .service
                    .set_config(&id, SessionConfigUpdate::Reasoning(reasoning))
                    .await?;
                ctx.reply_text("Reasoning updated").await?;
            }
            "/policy" => {
                let id = self.selected_session_id().await?;
                if let Some(value) = args.next() {
                    let mode = SessionMode::parse(value).map_err(anyhow::Error::msg)?;
                    self.host
                        .service
                        .set_config(&id, SessionConfigUpdate::Mode(mode))
                        .await?;
                    ctx.reply_text(&format!("Policy updated: {}", policy_name(mode)))
                        .await?;
                } else {
                    let snapshot = self
                        .host
                        .service
                        .load(&id)
                        .await?
                        .attach(self.endpoint(&ctx.from))
                        .await?
                        .snapshot;
                    ctx.reply_text(&format!(
                        "Policy: {}\nOptions: full_access | confirm | watch",
                        policy_name(snapshot.record.info.mode)
                    ))
                    .await?;
                }
            }
            "/allow" | "/deny" => {
                let request_id = args.next().context("permission id is required")?;
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
            let agent = self.host.create_session(None, None).await?;
            self.select_session(agent.id().as_str()).await?;
            agent
        };
        let content = self.prompt_content(ctx, text, agent.id()).await?;
        let endpoint = self.endpoint(&ctx.from);
        self.ensure_observer(agent.clone()).await?;
        agent.prompt_content(endpoint, content).await?;
        Ok(())
    }

    async fn prompt_content(
        &self,
        ctx: &MessageContext,
        text: &str,
        session_id: &SessionId,
    ) -> Result<MessageContent> {
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::text(text));
        }
        if let Some(media) = &ctx.media {
            if !self.media_input {
                bail!("Weixin media input is disabled");
            }
            let path = self.download_media(ctx, media, session_id).await?;
            blocks.push(resource_link_for_media(&path, media.media_type)?);
        }
        Ok(MessageContent::blocks(blocks))
    }

    async fn download_media(
        &self,
        ctx: &MessageContext,
        media: &MediaInfo,
        session_id: &SessionId,
    ) -> Result<PathBuf> {
        let now = Local::now();
        let directory = self
            .host
            .profile_root_path()
            .join("runtime")
            .join("attachments")
            .join("weixin")
            .join(format!("{:04}", now.year()))
            .join(format!("{:02}", now.month()))
            .join(format!("{:02}", now.day()))
            .join(session_id.as_str());
        tokio::fs::create_dir_all(&directory).await?;
        let filename = media
            .file_name
            .as_deref()
            .map(sanitize_filename)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| default_media_filename(media.media_type));
        let destination = unique_attachment_path(&directory, &filename).await?;
        Ok(ctx.download_media(media, &destination).await?)
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
        let task = tokio::spawn(stream_session(client, target, subscription));
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

fn split_command_line(input: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for character in input.chars() {
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
            started = true;
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                started = true;
            }
            character if character.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(character);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("unterminated quote in command");
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

fn parse_new_args(args: &[String]) -> Result<(Option<String>, Option<PathBuf>)> {
    let mut title = Vec::new();
    let mut cwd = None;
    let mut index = 0usize;
    while index < args.len() {
        let value = &args[index];
        if value == "--cwd" {
            index += 1;
            let path = args
                .get(index)
                .context("usage: /new [name] [--cwd <path>]")?;
            if cwd.replace(PathBuf::from(path)).is_some() {
                bail!("--cwd may only be specified once");
            }
        } else if let Some(path) = value.strip_prefix("--cwd=") {
            if path.is_empty() {
                bail!("usage: /new [name] [--cwd <path>]");
            }
            if cwd.replace(PathBuf::from(path)).is_some() {
                bail!("--cwd may only be specified once");
            }
        } else if value.starts_with("--") {
            bail!("unknown /new option: {value}");
        } else {
            title.push(value.as_str());
        }
        index += 1;
    }
    let title = (!title.is_empty()).then(|| title.join(" "));
    Ok((title, cwd))
}

fn policy_name(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::FullAccess => "full_access",
        SessionMode::Confirm => "confirm",
        SessionMode::Watch => "watch",
    }
}

async fn unique_attachment_path(directory: &Path, filename: &str) -> Result<PathBuf> {
    let original = directory.join(filename);
    if !tokio::fs::try_exists(&original).await? {
        return Ok(original);
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let extension = path.extension().and_then(|value| value.to_str());
    for index in 2..=u32::MAX {
        let candidate = match extension {
            Some(extension) => directory.join(format!("{stem}-{index}.{extension}")),
            None => directory.join(format!("{stem}-{index}")),
        };
        if !tokio::fs::try_exists(&candidate).await? {
            return Ok(candidate);
        }
    }
    bail!("could not allocate a unique attachment filename")
}

fn sanitize_filename(raw: &str) -> String {
    let mut sanitized = raw
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    sanitized = sanitized
        .trim_matches(|character| matches!(character, '.' | ' '))
        .to_string();
    let stem = Path::new(&sanitized)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        sanitized.insert(0, '_');
    }
    sanitized
}

fn default_media_filename(media_type: MediaType) -> String {
    match media_type {
        MediaType::Image => "image.jpg",
        MediaType::Video => "video.mp4",
        MediaType::Voice => "voice.dat",
        MediaType::File => "file.bin",
    }
    .to_string()
}

fn resource_link_for_media(path: &Path, media_type: MediaType) -> Result<ContentBlock> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("resolve downloaded media {}", path.display()))?;
    let metadata = std::fs::metadata(&path)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("attachment")
        .to_string();
    let mime_type = media_mime_type(&path, media_type).to_string();
    let size = i64::try_from(metadata.len()).ok();
    Ok(ContentBlock::ResourceLink {
        uri: file_uri_from_path(&path),
        name,
        mime_type: Some(mime_type),
        title: None,
        description: Some(format!("Local path: {}", display_path(&path))),
        size,
        annotations: None,
        meta: None,
    })
}

fn media_mime_type(path: &Path, media_type: MediaType) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("md") | Some("markdown") => "text/markdown",
        Some("json") => "application/json",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        _ => match media_type {
            MediaType::Image => "image/jpeg",
            MediaType::Voice => "audio/mpeg",
            MediaType::Video => "video/mp4",
            MediaType::File => "application/octet-stream",
        },
    }
}

fn file_uri_from_path(path: &Path) -> String {
    let normalized = display_path(path).replace('\\', "/");
    let encoded = normalized
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'.' | b'-' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect::<String>();
    if encoded.starts_with("//") {
        format!("file:{encoded}")
    } else if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

fn display_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if let Some(path) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = raw.strip_prefix(r"\\?\") {
        path.to_string()
    } else {
        raw.into_owned()
    }
}

async fn stream_session(
    client: Arc<WeixinClient>,
    target: String,
    mut subscription: dwo_agent_service::SessionSubscription,
) {
    let mut stream = WeixinStreamState::default();
    loop {
        let Some(event) = subscription.events.recv().await else {
            break;
        };
        match event.payload {
            SessionEventPayload::AssistantCompleted {
                content,
                tool_calls,
                ..
            } => {
                stream.remember_response(content);
                for call in tool_calls {
                    stream.remember_tool(call);
                }
            }
            SessionEventPayload::ToolStarted { call, .. } => stream.remember_tool(call),
            SessionEventPayload::PermissionRequested { permission, .. } => {
                if !stream.mark_permission_sent(&permission.tool_call_id) {
                    continue;
                }
                let call = stream
                    .tool(&permission.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| dwo_agent_service::ActiveToolCall {
                        tool_call_id: permission.tool_call_id.clone(),
                        tool_name: permission.tool_name.clone(),
                        raw_input: serde_json::Value::Null,
                    });
                send_logical_message(
                    &client,
                    &target,
                    &render_tool_call(&call, "request id", &permission.request_id),
                )
                .await;
            }
            SessionEventPayload::ToolCompleted { result, .. } => {
                stream.forget_tool(&result.tool_call_id);
            }
            SessionEventPayload::TurnCompleted { .. } => {
                if let Some(response) = stream.take_response() {
                    send_logical_message(&client, &target, &response).await;
                }
                stream.finish_turn();
            }
            SessionEventPayload::TurnCancelled { .. } => {
                stream.remember_response("Turn cancelled".to_string());
                if let Some(response) = stream.take_response() {
                    send_logical_message(&client, &target, &response).await;
                }
                stream.finish_turn();
            }
            SessionEventPayload::TurnFailed { error, .. } => {
                stream.remember_response(format!("Turn failed: {error}"));
                if let Some(response) = stream.take_response() {
                    send_logical_message(&client, &target, &response).await;
                }
                stream.finish_turn();
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct WeixinStreamState {
    responses: Vec<String>,
    tools: HashMap<String, dwo_agent_service::ActiveToolCall>,
    permission_messages: HashSet<String>,
}

impl WeixinStreamState {
    fn remember_response(&mut self, content: String) {
        let content = content.trim();
        if !content.is_empty() {
            self.responses.push(content.to_string());
        }
    }

    fn take_response(&mut self) -> Option<String> {
        let response = std::mem::take(&mut self.responses).join("\n\n");
        (!response.is_empty()).then_some(response)
    }

    fn remember_tool(&mut self, call: dwo_agent_service::ActiveToolCall) {
        self.tools.entry(call.tool_call_id.clone()).or_insert(call);
    }

    fn tool(&self, tool_call_id: &str) -> Option<&dwo_agent_service::ActiveToolCall> {
        self.tools.get(tool_call_id)
    }

    fn mark_permission_sent(&mut self, tool_call_id: &str) -> bool {
        self.permission_messages.insert(tool_call_id.to_string())
    }

    fn forget_tool(&mut self, tool_call_id: &str) {
        self.tools.remove(tool_call_id);
        self.permission_messages.remove(tool_call_id);
    }

    fn finish_turn(&mut self) {
        self.responses.clear();
        self.tools.clear();
        self.permission_messages.clear();
    }
}

fn render_tool_call(call: &dwo_agent_service::ActiveToolCall, id_label: &str, id: &str) -> String {
    let content = match call.tool_name.as_str() {
        "terminal" | "terminal_exec" => call
            .raw_input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        "file_edit" => call
            .raw_input
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
    .unwrap_or_else(|| {
        serde_json::to_string_pretty(&call.raw_input).unwrap_or_else(|_| call.tool_name.clone())
    });
    format!("🔧Tool Call:\n{}\n{id_label}：{id}", fenced(&content))
}

fn fenced(content: &str) -> String {
    let mut longest_run = 0usize;
    let mut current_run = 0usize;
    for character in content.chars() {
        if character == '`' {
            current_run += 1;
            longest_run = longest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    let fence = "`".repeat(longest_run.saturating_add(1).max(3));
    format!("{fence}\n{content}\n{fence}")
}

const WEIXIN_TEXT_CHUNK_CHARS: usize = 4000;

fn split_logical_message(text: &str) -> Vec<String> {
    let mut remaining = text.trim();
    let mut chunks = Vec::new();
    while remaining.chars().count() > WEIXIN_TEXT_CHUNK_CHARS {
        let hard_boundary = remaining
            .char_indices()
            .nth(WEIXIN_TEXT_CHUNK_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        let preferred_boundary = remaining[..hard_boundary]
            .rfind("\n\n")
            .map(|index| index + 2)
            .filter(|index| remaining[..*index].chars().count() >= WEIXIN_TEXT_CHUNK_CHARS / 2);
        let boundary = preferred_boundary.unwrap_or(hard_boundary);
        let chunk = remaining[..boundary].trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        remaining = remaining[boundary..].trim_start();
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }
    chunks
}

async fn send_logical_message(client: &WeixinClient, target: &str, text: &str) {
    for chunk in split_logical_message(text) {
        send(client, target, &chunk).await;
    }
}

async fn reply_logical_message(ctx: &MessageContext, text: &str) -> Result<()> {
    for chunk in split_logical_message(text) {
        ctx.reply_text(&chunk).await?;
    }
    Ok(())
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

fn render_status(snapshot: &dwo_agent_service::SessionSnapshot) -> String {
    let mut lines = vec![format!(
        "Session: {}\nCwd: {}\nPolicy: {}\nModel: {}\nReasoning: {}\nState: {:?}",
        snapshot.record.info.title,
        display_path(&snapshot.record.info.cwd),
        policy_name(snapshot.record.info.mode),
        snapshot.record.llm.model,
        snapshot
            .record
            .llm
            .reasoning
            .as_deref()
            .unwrap_or("default"),
        snapshot.phase,
    )];
    if !snapshot.partial_message.is_empty() {
        lines.push(format!("Current: {}", snapshot.partial_message));
    }
    if let Some(permission) = &snapshot.pending_permission {
        lines.push(format!("Pending permission: {}", permission.request_id));
    }
    lines.join("\n")
}

struct ReplayTurn {
    turn_id: dwo_agent_service::TurnId,
    prompts: Vec<String>,
    responses: Vec<String>,
}

fn render_replay_turns(transcript: &[ClientTranscriptEvent], turns: usize) -> Vec<String> {
    let mut grouped = Vec::<ReplayTurn>::new();
    for event in transcript {
        let turn_id = match &event.payload {
            SessionEventPayload::UserPromptSubmitted { turn_id, .. }
            | SessionEventPayload::AssistantCompleted { turn_id, .. } => turn_id,
            _ => continue,
        };
        if grouped.last().is_none_or(|turn| turn.turn_id != *turn_id) {
            grouped.push(ReplayTurn {
                turn_id: turn_id.clone(),
                prompts: Vec::new(),
                responses: Vec::new(),
            });
        }
        let turn = grouped.last_mut().expect("replay turn was just inserted");
        match &event.payload {
            SessionEventPayload::UserPromptSubmitted { content, .. } => {
                turn.prompts.push(content.to_string())
            }
            SessionEventPayload::AssistantCompleted { content, .. }
                if !content.trim().is_empty() =>
            {
                turn.responses.push(content.trim().to_string());
            }
            _ => {}
        }
    }

    let rendered = grouped
        .into_iter()
        .filter_map(|turn| {
            if turn.prompts.is_empty() && turn.responses.is_empty() {
                return None;
            }
            let mut sections = Vec::new();
            if !turn.prompts.is_empty() {
                sections.push(format!("User:\n{}", turn.prompts.join("\n\n")));
            }
            if !turn.responses.is_empty() {
                sections.push(format!("Assistant:\n{}", turn.responses.join("\n\n")));
            }
            Some(sections.join("\n\n"))
        })
        .collect::<Vec<_>>();
    rendered[rendered.len().saturating_sub(turns)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_are_buffered_in_commit_order() {
        let mut stream = WeixinStreamState::default();
        stream.remember_response("先检查项目".to_string());
        stream.remember_response("  ".to_string());
        stream.remember_response("检查完成".to_string());

        assert_eq!(
            stream.take_response().as_deref(),
            Some("先检查项目\n\n检查完成")
        );
        assert_eq!(stream.take_response(), None);
    }

    #[test]
    fn replay_groups_user_and_responses_by_turn() {
        let first = dwo_agent_service::TurnId::new();
        let second = dwo_agent_service::TurnId::new();
        let transcript = vec![
            ClientTranscriptEvent::new(SessionEventPayload::UserPromptSubmitted {
                turn_id: first.clone(),
                origin: EndpointId::new(),
                content: MessageContent::text("first question"),
            }),
            ClientTranscriptEvent::new(SessionEventPayload::AssistantCompleted {
                turn_id: first.clone(),
                content: "intermediate".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
            }),
            ClientTranscriptEvent::new(SessionEventPayload::AssistantCompleted {
                turn_id: first,
                content: "first answer".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
            }),
            ClientTranscriptEvent::new(SessionEventPayload::UserPromptSubmitted {
                turn_id: second.clone(),
                origin: EndpointId::new(),
                content: MessageContent::text("second question"),
            }),
            ClientTranscriptEvent::new(SessionEventPayload::AssistantCompleted {
                turn_id: second,
                content: "second answer".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
            }),
        ];

        assert_eq!(
            render_replay_turns(&transcript, 1),
            vec!["User:\nsecond question\n\nAssistant:\nsecond answer"]
        );
        assert_eq!(
            render_replay_turns(&transcript, 10)[0],
            "User:\nfirst question\n\nAssistant:\nintermediate\n\nfirst answer"
        );
    }

    #[test]
    fn logical_messages_split_at_four_thousand_unicode_characters() {
        let text = format!("{}\n\n{}", "前".repeat(3_000), "后".repeat(2_000));
        let chunks = split_logical_message(&text);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 3_000);
        assert_eq!(chunks[1].chars().count(), 2_000);
        assert_eq!(
            chunks.concat(),
            format!("{}{}", "前".repeat(3_000), "后".repeat(2_000))
        );
    }

    #[test]
    fn terminal_and_file_edit_calls_render_their_useful_arguments() {
        let terminal = dwo_agent_service::ActiveToolCall {
            tool_call_id: "call-terminal".to_string(),
            tool_name: "terminal".to_string(),
            raw_input: json!({"action":"run", "command":"ls -a"}),
        };
        let rendered = render_tool_call(&terminal, "request id", "permission-1");
        assert_eq!(
            rendered,
            "🔧Tool Call:\n```\nls -a\n```\nrequest id：permission-1"
        );

        let file_edit = dwo_agent_service::ActiveToolCall {
            tool_call_id: "call-edit".to_string(),
            tool_name: "file_edit".to_string(),
            raw_input: json!({"patch":"*** Begin Patch\n```\n*** End Patch"}),
        };
        let rendered = render_tool_call(&file_edit, "tool call id", "call-edit");
        assert!(rendered.contains("````\n*** Begin Patch\n```\n*** End Patch\n````"));
    }

    #[test]
    fn new_command_supports_quoted_windows_cwd_and_multiword_title() {
        let tokens = split_command_line(
            r#"/new Project review --cwd "C:\Users\Example User\Documents\repo""#,
        )
        .unwrap();
        let (title, cwd) = parse_new_args(&tokens[1..]).unwrap();

        assert_eq!(title.as_deref(), Some("Project review"));
        assert_eq!(
            cwd.as_deref(),
            Some(Path::new(r"C:\Users\Example User\Documents\repo"))
        );
    }

    #[test]
    fn media_resource_includes_file_uri_mime_size_and_local_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("paper example.pdf");
        std::fs::write(&path, b"pdf").unwrap();

        let block = resource_link_for_media(&path, MediaType::File).unwrap();
        let ContentBlock::ResourceLink {
            uri,
            mime_type,
            description,
            size,
            ..
        } = block
        else {
            panic!("expected resource link");
        };
        assert!(uri.ends_with("paper%20example.pdf"));
        assert_eq!(mime_type.as_deref(), Some("application/pdf"));
        assert_eq!(size, Some(3));
        assert!(description.unwrap().contains("paper example.pdf"));
    }

    #[test]
    fn windows_verbatim_paths_are_hidden_in_user_facing_text_and_uris() {
        let path = Path::new(r"\\?\C:\Users\Example User\paper.pdf");

        assert_eq!(display_path(path), r"C:\Users\Example User\paper.pdf");
        assert_eq!(
            file_uri_from_path(path),
            "file:///C:/Users/Example%20User/paper.pdf"
        );
    }
}
