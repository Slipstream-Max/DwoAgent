//! ACP-over-WebSocket ingress channel.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::ByteStreams;
use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use uuid::Uuid;

use super::acp::{AcpSessionLeaseBridge, run_acp_transport_with_leases};
use super::bridge::SessionLeaseRegistry;
use super::config::{WebSocketIngressConfig, load_channel_runtime_config};
use crate::agent::service::AgentService;
use crate::config::loader::{channel_secret_dir, resolve_agent_structure_dir};
use crate::utils::files::read_utf8_text;

const BRIDGE_BUFFER_SIZE: usize = 64 * 1024;
const WEBSOCKET_SECRET_SUBDIR: &str = "websocket";
const WEBSOCKET_AUTH_FILE: &str = "auth.yaml";
const TOKEN_PREFIX: &str = "dwo_ws_";
const MIN_TOKEN_LEN: usize = TOKEN_PREFIX.len() + 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSocketAuth {
    token: String,
}

pub struct WebSocketChannel {
    agent: Arc<AgentService>,
    leases: Arc<SessionLeaseRegistry>,
    bind_addr: String,
    auth_token: Option<String>,
}

impl WebSocketChannel {
    pub fn new(
        agent: Arc<AgentService>,
        leases: Arc<SessionLeaseRegistry>,
        agent_structure_dir: &Path,
        config: &WebSocketIngressConfig,
    ) -> Result<Self> {
        let auth_token = if config.auth {
            let path = auth_path(agent_structure_dir);
            Some(read_auth(&path)?.token)
        } else {
            None
        };
        Ok(Self {
            agent,
            leases,
            bind_addr: config.bind_addr.clone(),
            auth_token,
        })
    }

    pub async fn run(self) -> Result<()> {
        let listener = TcpListener::bind(&self.bind_addr)
            .await
            .with_context(|| format!("bind websocket ingress at {}", self.bind_addr))?;
        tracing::info!("ACP websocket ingress listening on {}", self.bind_addr);

        loop {
            let (stream, peer_addr) = listener.accept().await.context("accept websocket tcp")?;
            let agent = self.agent.clone();
            let leases = self.leases.clone();
            let auth_token = self.auth_token.clone();
            tokio::spawn(async move {
                if let Err(err) = serve_connection(agent, leases, stream, auth_token).await {
                    tracing::warn!(%peer_addr, error = %format!("{err:#}"), "ACP websocket connection ended with error");
                }
            });
        }
    }
}

pub fn run_websocket_login_sync(agent_folder: PathBuf) -> Result<()> {
    run_websocket_login(&agent_folder)
}

pub fn run_websocket_login(agent_folder: &Path) -> Result<()> {
    let agent_structure_dir = resolve_agent_structure_dir(agent_folder)?;
    let config = load_channel_runtime_config(&agent_structure_dir)?;
    if !config.websocket.enabled {
        bail!("websocket channel is not enabled in agent.yaml `channels.websocket`.");
    }
    if !config.websocket.auth {
        bail!(
            "websocket auth is disabled in agent.yaml; set `channels.websocket.auth: true` first."
        );
    }

    let auth = WebSocketAuth {
        token: generate_token(),
    };
    let path = auth_path(&agent_structure_dir);
    write_auth(&path, &auth)?;

    println!("WebSocket token:");
    println!("{}", auth.token);
    println!("WebSocket credentials saved to {}", path.display());
    Ok(())
}

async fn serve_connection(
    agent: Arc<AgentService>,
    leases: Arc<SessionLeaseRegistry>,
    stream: TcpStream,
    auth_token: Option<String>,
) -> Result<()> {
    let websocket = accept_hdr_async(stream, move |request: &Request, response: Response| {
        if let Some(expected) = auth_token.as_deref()
            && !request_is_authorized(request, expected)
        {
            return Err(unauthorized_response());
        }
        Ok(response)
    })
    .await
    .context("accept websocket")?;
    let (ws_writer, ws_reader) = websocket.split();

    let (incoming_writer, incoming_reader) = tokio::io::duplex(BRIDGE_BUFFER_SIZE);
    let (outgoing_writer, outgoing_reader) = tokio::io::duplex(BRIDGE_BUFFER_SIZE);

    let mut incoming_task = tokio::spawn(websocket_to_acp_lines(ws_reader, incoming_writer));
    let mut outgoing_task = tokio::spawn(acp_lines_to_websocket(outgoing_reader, ws_writer));
    let transport = ByteStreams::new(outgoing_writer.compat_write(), incoming_reader.compat());
    let holder = format!("websocket:{}", Uuid::new_v4().simple());
    let bridge = AcpSessionLeaseBridge::new(holder, leases);
    let mut acp_task = tokio::spawn(run_acp_transport_with_leases(
        agent,
        transport,
        Some(bridge),
    ));

    let result = tokio::select! {
        result = &mut acp_task => join_acp_result(result).context("run ACP websocket transport"),
        result = &mut incoming_task => join_bridge_result(result).context("read websocket ACP input"),
        result = &mut outgoing_task => join_bridge_result(result).context("write websocket ACP output"),
    };

    abort_task(acp_task);
    abort_task(incoming_task);
    abort_task(outgoing_task);

    result
}

async fn websocket_to_acp_lines(
    mut ws_reader: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    >,
    mut acp_writer: tokio::io::DuplexStream,
) -> Result<()> {
    while let Some(message) = ws_reader.next().await {
        let message = message.context("read websocket message")?;
        match message {
            Message::Text(text) => write_ws_payload_as_acp_line(&mut acp_writer, text.as_bytes())
                .await
                .context("forward websocket text message")?,
            Message::Binary(bytes) => write_ws_payload_as_acp_line(&mut acp_writer, &bytes)
                .await
                .context("forward websocket binary message")?,
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    acp_writer.shutdown().await.context("close ACP input")?;
    Ok(())
}

async fn write_ws_payload_as_acp_line(
    writer: &mut tokio::io::DuplexStream,
    payload: &[u8],
) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }
    writer.write_all(payload).await?;
    if !payload.ends_with(b"\n") {
        writer.write_all(b"\n").await?;
    }
    writer.flush().await?;
    Ok(())
}

async fn acp_lines_to_websocket(
    acp_reader: tokio::io::DuplexStream,
    mut ws_writer: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        Message,
    >,
) -> Result<()> {
    let mut lines = BufReader::new(acp_reader).lines();
    while let Some(line) = lines.next_line().await.context("read ACP output line")? {
        if line.trim().is_empty() {
            continue;
        }
        ws_writer
            .send(Message::Text(line))
            .await
            .context("send websocket message")?;
    }
    let _ = ws_writer.close().await;
    Ok(())
}

fn join_acp_result(
    result: std::result::Result<
        std::result::Result<(), agent_client_protocol::Error>,
        tokio::task::JoinError,
    >,
) -> Result<()> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(anyhow::anyhow!("ACP connection error: {err}")),
        Err(err) => Err(err).context("join ACP task"),
    }
}

fn join_bridge_result(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Ok(result) => result,
        Err(err) => Err(err).context("join websocket bridge task"),
    }
}

fn abort_task<T>(task: JoinHandle<T>) {
    if !task.is_finished() {
        task.abort();
    }
}

fn auth_path(agent_structure_dir: &Path) -> PathBuf {
    channel_secret_dir(agent_structure_dir)
        .join(WEBSOCKET_SECRET_SUBDIR)
        .join(WEBSOCKET_AUTH_FILE)
}

fn read_auth(path: &Path) -> Result<WebSocketAuth> {
    if !path.is_file() {
        bail!(
            "WebSocket auth file not found: {}. Run `dwo-agent channel login websocket --agent-folder <agent-folder>` first.",
            path.display()
        );
    }
    let text = read_utf8_text(path)?;
    let auth: WebSocketAuth =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    validate_auth(&auth)?;
    Ok(auth)
}

fn write_auth(path: &Path, auth: &WebSocketAuth) -> Result<()> {
    validate_auth(auth)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_yaml::to_string(auth)?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn validate_auth(auth: &WebSocketAuth) -> Result<()> {
    let token = auth.token.trim();
    if token.len() < MIN_TOKEN_LEN || !token.starts_with(TOKEN_PREFIX) {
        bail!(
            "WebSocket token is invalid; run `dwo-agent channel login websocket` to generate a new token."
        );
    }
    Ok(())
}

fn generate_token() -> String {
    format!(
        "{TOKEN_PREFIX}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

fn request_is_authorized(request: &Request, expected_token: &str) -> bool {
    let Some(header) = request.headers().get("authorization") else {
        return false;
    };
    let Ok(value) = header.to_str() else {
        return false;
    };
    let Some((scheme, token)) = value.trim().split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return false;
    }
    constant_time_eq(token.trim().as_bytes(), expected_token.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let max_len = a.len().max(b.len());
    for index in 0..max_len {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff == 0
}

fn unauthorized_response() -> ErrorResponse {
    http_response(
        StatusCode::UNAUTHORIZED,
        "missing or invalid websocket bearer token",
    )
}

fn http_response(status: StatusCode, body: &str) -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(status)
        .body(Some(body.to_string()))
        .unwrap_or_else(|_| {
            tokio_tungstenite::tungstenite::http::Response::new(Some(body.to_string()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_login_writes_valid_token() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = write_agent_structure(tmp.path(), true, true);

        run_websocket_login(&agent_dir).unwrap();

        let auth = read_auth(&auth_path(&agent_dir)).unwrap();
        assert!(auth.token.starts_with(TOKEN_PREFIX));
        assert!(auth.token.len() >= MIN_TOKEN_LEN);
    }

    #[test]
    fn websocket_login_requires_enabled_auth_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let disabled_dir = write_agent_structure(tmp.path(), false, true);
        let disabled_err = run_websocket_login(&disabled_dir).unwrap_err();
        assert!(disabled_err.to_string().contains("not enabled"));

        let no_auth_dir = tmp.path().join("no-auth-agent");
        write_agent_structure_at(&no_auth_dir, true, false);
        let no_auth_err = run_websocket_login(&no_auth_dir).unwrap_err();
        assert!(no_auth_err.to_string().contains("auth is disabled"));
    }

    #[test]
    fn constant_time_eq_matches_expected_values() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"same", b"same-longer"));
    }

    fn write_agent_structure(
        root: &Path,
        websocket_enabled: bool,
        websocket_auth: bool,
    ) -> PathBuf {
        let agent_dir = root.join("agent");
        write_agent_structure_at(&agent_dir, websocket_enabled, websocket_auth);
        agent_dir
    }

    fn write_agent_structure_at(agent_dir: &Path, websocket_enabled: bool, websocket_auth: bool) {
        let prompt_dir = agent_dir.join("resources").join("prompt");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        std::fs::write(
            agent_dir.join("agent.yaml"),
            format!(
                "\
agent_id: websocket-test
name: websocket-test
description: websocket auth test
policy_mode: full_access
model:
  default_model_id: mock
  models:
    - model_name: mock
      provider: deepseek
      model_id: deepseek-v4-pro
      api_key: test
channels:
  websocket:
    enabled: {websocket_enabled}
    auth: {websocket_auth}
"
            ),
        )
        .unwrap();
        std::fs::write(
            prompt_dir.join("system.md"),
            "You are a websocket test agent.",
        )
        .unwrap();
    }
}
