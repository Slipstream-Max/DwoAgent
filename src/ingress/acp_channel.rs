//! Long-lived ACP local channel and short-lived stdio bridge.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::ByteStreams;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use uuid::Uuid;

use super::acp::run_acp_transport;
use super::config::{AcpChannelConfig, load_channel_runtime_config};
use crate::agent::service::AgentService;
use crate::config::loader::{resolve_agent_structure_dir, utc_iso};
use crate::utils::files::read_utf8_text;

const ACP_SECRET_DIR: &str = "channel_secret/acp";
const ACP_AUTH_FILE: &str = "auth.yaml";
const ACP_DAEMON_FILE: &str = "daemon.yaml";
const TOKEN_PREFIX: &str = "dwo_acp_";
const MIN_TOKEN_LEN: usize = TOKEN_PREFIX.len() + 64;
const AUTH_LINE_PREFIX: &str = "DWO_AUTH ";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcpAuth {
    token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcpIpcKind {
    NamedPipe,
    UnixSocket,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcpIpcEndpoint {
    pub kind: AcpIpcKind,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpDaemonManifest {
    pub pid: u32,
    pub started_at: String,
    pub transport: String,
    pub ipc: AcpIpcEndpoint,
}

pub struct AcpChannel {
    agent: Arc<AgentService>,
    agent_structure_dir: PathBuf,
    config: AcpChannelConfig,
    auth_token: Option<String>,
    endpoint: AcpIpcEndpoint,
}

impl AcpChannel {
    pub fn new(
        agent: Arc<AgentService>,
        agent_structure_dir: &Path,
        config: &AcpChannelConfig,
    ) -> Result<Self> {
        if !config.ipc {
            bail!("acp channel is enabled but `acp.ipc` is false; no transport is configured.");
        }
        let auth_token = if config.auth {
            Some(read_auth(&auth_path(agent_structure_dir))?.token)
        } else {
            None
        };
        let endpoint = default_ipc_endpoint(agent_structure_dir)?;
        Ok(Self {
            agent,
            agent_structure_dir: agent_structure_dir.to_path_buf(),
            config: config.clone(),
            auth_token,
            endpoint,
        })
    }

    pub async fn run(self) -> Result<()> {
        if !self.config.ipc {
            bail!("acp channel is enabled but `acp.ipc` is false; no transport is configured.");
        }
        run_ipc_listener(
            self.agent,
            self.agent_structure_dir,
            self.endpoint,
            self.auth_token,
        )
        .await
    }
}

pub fn run_acp_login_sync(agent_folder: PathBuf) -> Result<()> {
    run_acp_login(&agent_folder)
}

pub fn run_acp_login(agent_folder: &Path) -> Result<()> {
    let agent_structure_dir = resolve_agent_structure_dir(agent_folder)?;
    let config = load_channel_runtime_config(&agent_structure_dir)?;
    if !config.acp.enabled {
        bail!("acp channel is not enabled in channels.yaml.");
    }
    if !config.acp.auth {
        bail!("acp auth is disabled in channels.yaml; set `acp.auth: true` first.");
    }

    let auth = AcpAuth {
        token: generate_token(),
    };
    let path = auth_path(&agent_structure_dir);
    write_auth(&path, &auth)?;

    println!("ACP token:");
    println!("{}", auth.token);
    println!("ACP credentials saved to {}", path.display());
    Ok(())
}

pub fn run_acp_connect_sync(agent_folder: PathBuf, ipc: Option<String>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_acp_connect(&agent_folder, ipc))
}

pub async fn run_acp_connect(agent_folder: &Path, ipc: Option<String>) -> Result<()> {
    let agent_structure_dir = resolve_agent_structure_dir(agent_folder)?;
    let config = load_channel_runtime_config(&agent_structure_dir)?;
    if !config.acp.enabled {
        bail!("acp channel is not enabled in channels.yaml.");
    }
    if !config.acp.ipc {
        bail!("acp channel has no IPC transport configured; set `acp.ipc: true`.");
    }

    let endpoint = match ipc {
        Some(name) => override_ipc_endpoint(name),
        None => read_daemon_manifest(&daemon_path(&agent_structure_dir))
            .with_context(|| {
                format!(
                    "No running dwo-agent serve found for {}. Start it with `dwo-agent serve --agent-folder {}`.",
                    agent_structure_dir.display(),
                    agent_folder.display()
                )
            })?
            .ipc,
    };
    let auth_token = if config.acp.auth {
        Some(read_auth(&auth_path(&agent_structure_dir))?.token)
    } else {
        None
    };

    connect_stdio_bridge(endpoint, auth_token).await
}

async fn connect_stdio_bridge(endpoint: AcpIpcEndpoint, auth_token: Option<String>) -> Result<()> {
    let stream = connect_ipc(&endpoint).await.with_context(|| {
        format!(
            "connect ACP IPC endpoint {} ({:?})",
            endpoint.name, endpoint.kind
        )
    })?;
    bridge_stdio_to_stream(stream, auth_token).await
}

async fn bridge_stdio_to_stream<S>(stream: S, auth_token: Option<String>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    if let Some(token) = auth_token {
        write_half
            .write_all(format!("{AUTH_LINE_PREFIX}{token}\n").as_bytes())
            .await
            .context("write ACP IPC auth line")?;
        write_half.flush().await.context("flush ACP IPC auth line")?;
    }

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let to_ipc = async {
        tokio::io::copy(&mut stdin, &mut write_half)
            .await
            .context("forward ACP stdin to IPC")?;
        write_half.shutdown().await.context("close ACP IPC input")
    };
    let from_ipc = async {
        tokio::io::copy(&mut read_half, &mut stdout)
            .await
            .context("forward ACP IPC output to stdout")?;
        stdout.flush().await.context("flush ACP stdout")
    };

    tokio::select! {
        result = to_ipc => result,
        result = from_ipc => result,
    }
}

async fn serve_ipc_connection<S>(
    agent: Arc<AgentService>,
    stream: S,
    auth_token: Option<String>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    if let Some(expected) = auth_token.as_deref() {
        read_auth_line(&mut reader, expected)
            .await
            .context("authenticate ACP IPC connection")?;
    }

    let transport = ByteStreams::new(write_half.compat_write(), reader.compat());
    run_acp_transport(agent, transport)
        .await
        .map_err(|err| anyhow::anyhow!("ACP IPC connection error: {err}"))
}

async fn read_auth_line<R>(reader: &mut R, expected_token: &str) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .await
        .context("read ACP IPC auth line")?;
    if read == 0 {
        bail!("ACP IPC connection closed before auth.");
    }
    let token = line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix(AUTH_LINE_PREFIX)
        .map(str::trim)
        .ok_or_else(|| anyhow::anyhow!("missing ACP IPC auth line"))?;
    if !constant_time_eq(token.as_bytes(), expected_token.as_bytes()) {
        bail!("invalid ACP IPC token");
    }
    Ok(())
}

fn auth_path(agent_structure_dir: &Path) -> PathBuf {
    agent_structure_dir.join(ACP_SECRET_DIR).join(ACP_AUTH_FILE)
}

fn daemon_path(agent_structure_dir: &Path) -> PathBuf {
    agent_structure_dir
        .join(ACP_SECRET_DIR)
        .join(ACP_DAEMON_FILE)
}

fn read_auth(path: &Path) -> Result<AcpAuth> {
    if !path.is_file() {
        bail!(
            "ACP auth file not found: {}. Run `dwo-agent channel login acp --agent-folder <agent-folder>` first.",
            path.display()
        );
    }
    let text = read_utf8_text(path)?;
    let auth: AcpAuth =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    validate_auth(&auth)?;
    Ok(auth)
}

fn write_auth(path: &Path, auth: &AcpAuth) -> Result<()> {
    validate_auth(auth)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_yaml::to_string(auth)?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn validate_auth(auth: &AcpAuth) -> Result<()> {
    let token = auth.token.trim();
    if token.len() < MIN_TOKEN_LEN || !token.starts_with(TOKEN_PREFIX) {
        bail!("ACP token is invalid; run `dwo-agent channel login acp` to generate a new token.");
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

fn read_daemon_manifest(path: &Path) -> Result<AcpDaemonManifest> {
    if !path.is_file() {
        bail!("ACP daemon manifest not found: {}", path.display());
    }
    let text = read_utf8_text(path)?;
    let manifest: AcpDaemonManifest =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(manifest)
}

fn write_daemon_manifest(path: &Path, endpoint: &AcpIpcEndpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let manifest = AcpDaemonManifest {
        pid: std::process::id(),
        started_at: utc_iso(),
        transport: "ipc".to_string(),
        ipc: endpoint.clone(),
    };
    let text = serde_yaml::to_string(&manifest)?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

struct DaemonManifestGuard {
    path: PathBuf,
}

impl DaemonManifestGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for DaemonManifestGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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

#[cfg(windows)]
fn default_ipc_endpoint(_agent_structure_dir: &Path) -> Result<AcpIpcEndpoint> {
    Ok(AcpIpcEndpoint {
        kind: AcpIpcKind::NamedPipe,
        name: format!(r"\\.\pipe\dwo-agent-{}", Uuid::new_v4().simple()),
    })
}

#[cfg(unix)]
fn default_ipc_endpoint(agent_structure_dir: &Path) -> Result<AcpIpcEndpoint> {
    let dir = agent_structure_dir.join(ACP_SECRET_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(AcpIpcEndpoint {
        kind: AcpIpcKind::UnixSocket,
        name: dir
            .join(format!("dwo-agent-{}.sock", Uuid::new_v4().simple()))
            .to_string_lossy()
            .into_owned(),
    })
}

#[cfg(windows)]
fn override_ipc_endpoint(name: String) -> AcpIpcEndpoint {
    AcpIpcEndpoint {
        kind: AcpIpcKind::NamedPipe,
        name,
    }
}

#[cfg(unix)]
fn override_ipc_endpoint(name: String) -> AcpIpcEndpoint {
    AcpIpcEndpoint {
        kind: AcpIpcKind::UnixSocket,
        name,
    }
}

#[cfg(windows)]
async fn run_ipc_listener(
    agent: Arc<AgentService>,
    agent_structure_dir: PathBuf,
    endpoint: AcpIpcEndpoint,
    auth_token: Option<String>,
) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    if !matches!(endpoint.kind, AcpIpcKind::NamedPipe) {
        bail!("Windows ACP IPC requires a named pipe endpoint.");
    }

    let manifest_path = daemon_path(&agent_structure_dir);
    let mut first = true;
    let mut manifest_guard: Option<DaemonManifestGuard> = None;
    loop {
        let server = ServerOptions::new()
            .first_pipe_instance(first)
            .create(&endpoint.name)
            .with_context(|| format!("create ACP named pipe {}", endpoint.name))?;
        if first {
            write_daemon_manifest(&manifest_path, &endpoint)?;
            manifest_guard = Some(DaemonManifestGuard::new(manifest_path.clone()));
            tracing::info!("ACP IPC channel listening on {}", endpoint.name);
            first = false;
        }
        server
            .connect()
            .await
            .with_context(|| format!("accept ACP named pipe {}", endpoint.name))?;
        let agent = agent.clone();
        let auth_token = auth_token.clone();
        tokio::spawn(async move {
            if let Err(err) = serve_ipc_connection(agent, server, auth_token).await {
                tracing::warn!(error = %format!("{err:#}"), "ACP IPC connection ended with error");
            }
        });
        let _ = manifest_guard.as_ref();
    }
}

#[cfg(unix)]
async fn run_ipc_listener(
    agent: Arc<AgentService>,
    agent_structure_dir: PathBuf,
    endpoint: AcpIpcEndpoint,
    auth_token: Option<String>,
) -> Result<()> {
    use tokio::net::UnixListener;

    if !matches!(endpoint.kind, AcpIpcKind::UnixSocket) {
        bail!("Unix ACP IPC requires a unix socket endpoint.");
    }
    let _ = std::fs::remove_file(&endpoint.name);
    let listener = UnixListener::bind(&endpoint.name)
        .with_context(|| format!("bind ACP unix socket {}", endpoint.name))?;
    let manifest_path = daemon_path(&agent_structure_dir);
    write_daemon_manifest(&manifest_path, &endpoint)?;
    let _manifest_guard = DaemonManifestGuard::new(manifest_path);
    tracing::info!("ACP IPC channel listening on {}", endpoint.name);

    loop {
        let (stream, _addr) = listener.accept().await.context("accept ACP unix socket")?;
        let agent = agent.clone();
        let auth_token = auth_token.clone();
        tokio::spawn(async move {
            if let Err(err) = serve_ipc_connection(agent, stream, auth_token).await {
                tracing::warn!(error = %format!("{err:#}"), "ACP IPC connection ended with error");
            }
        });
    }
}

#[cfg(windows)]
async fn connect_ipc(
    endpoint: &AcpIpcEndpoint,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;

    if !matches!(endpoint.kind, AcpIpcKind::NamedPipe) {
        bail!("daemon manifest is not a named pipe endpoint.");
    }
    ClientOptions::new()
        .open(&endpoint.name)
        .with_context(|| format!("open ACP named pipe {}", endpoint.name))
}

#[cfg(unix)]
async fn connect_ipc(endpoint: &AcpIpcEndpoint) -> Result<tokio::net::UnixStream> {
    use tokio::net::UnixStream;

    if !matches!(endpoint.kind, AcpIpcKind::UnixSocket) {
        bail!("daemon manifest is not a unix socket endpoint.");
    }
    UnixStream::connect(&endpoint.name)
        .await
        .with_context(|| format!("connect ACP unix socket {}", endpoint.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_login_writes_valid_token() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = write_agent_structure(tmp.path(), true, true);

        run_acp_login(&agent_dir).unwrap();

        let auth = read_auth(&auth_path(&agent_dir)).unwrap();
        assert!(auth.token.starts_with(TOKEN_PREFIX));
        assert!(auth.token.len() >= MIN_TOKEN_LEN);
    }

    #[test]
    fn acp_login_requires_enabled_auth_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let disabled_dir = write_agent_structure(tmp.path(), false, true);
        let disabled_err = run_acp_login(&disabled_dir).unwrap_err();
        assert!(disabled_err.to_string().contains("not enabled"));

        let no_auth_dir = tmp.path().join("no-auth-agent");
        write_agent_structure_at(&no_auth_dir, true, false);
        let no_auth_err = run_acp_login(&no_auth_dir).unwrap_err();
        assert!(no_auth_err.to_string().contains("auth is disabled"));
    }

    #[test]
    fn daemon_manifest_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = AcpIpcEndpoint {
            kind: AcpIpcKind::NamedPipe,
            name: r"\\.\pipe\dwo-agent-test".to_string(),
        };
        let path = tmp.path().join("daemon.yaml");

        write_daemon_manifest(&path, &endpoint).unwrap();
        let manifest = read_daemon_manifest(&path).unwrap();

        assert_eq!(manifest.transport, "ipc");
        assert_eq!(manifest.ipc, endpoint);
        assert!(manifest.pid > 0);
    }

    #[test]
    fn constant_time_eq_matches_expected_values() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"same", b"same-longer"));
    }

    fn write_agent_structure(root: &Path, acp_enabled: bool, acp_auth: bool) -> PathBuf {
        let agent_dir = root.join("agent");
        write_agent_structure_at(&agent_dir, acp_enabled, acp_auth);
        agent_dir
    }

    fn write_agent_structure_at(agent_dir: &Path, acp_enabled: bool, acp_auth: bool) {
        std::fs::create_dir_all(agent_dir.join("resources").join("agents")).unwrap();
        std::fs::write(
            agent_dir.join("agent.yaml"),
            "\
agent_id: acp-test
name: acp-test
description: acp auth test
policy_mode: full_access
",
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("model.yaml"),
            "\
default_model_id: mock
models:
  - model_name: mock
    provider: deepseek
    model_id: deepseek-v4-pro
    api_key: test
",
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("channels.yaml"),
            format!(
                "\
acp:
  enabled: {acp_enabled}
  ipc: true
  auth: {acp_auth}
"
            ),
        )
        .unwrap();
    }
}
