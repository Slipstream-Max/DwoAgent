use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::host::Host;
use crate::local::ipc_schema::{RpcRequest, RpcResponse};

// TODO(gui): version these DTOs and add structured errors/capabilities before Flutter consumes IPC.

pub fn endpoint(_config_path: &Path) -> String {
    #[cfg(windows)]
    {
        r"\\.\pipe\dwoagent".to_string()
    }
    #[cfg(unix)]
    {
        let base = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("dwoagent.sock").to_string_lossy().into_owned()
    }
}

pub async fn serve(host: Arc<Host>, config_path: &Path) -> Result<()> {
    let endpoint = endpoint(config_path);
    #[cfg(windows)]
    {
        serve_windows(host, &endpoint).await
    }
    #[cfg(unix)]
    {
        serve_unix(host, Path::new(&endpoint)).await
    }
}

#[cfg(windows)]
async fn serve_windows(host: Arc<Host>, endpoint: &str) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut first = true;
    loop {
        let mut options = ServerOptions::new();
        if first {
            options.first_pipe_instance(true);
            first = false;
        }
        let server = options
            .create(endpoint)
            .with_context(|| format!("listen on {endpoint}"))?;
        let shutdown = host.shutdown_token();
        tokio::select! {
            result = server.connect() => result?,
            _ = shutdown.cancelled() => break,
        }
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(server, host).await {
                tracing::warn!(
                    event = "ipc.connection_failed",
                    error = %format!("{error:#}"),
                    "IPC connection failed"
                );
            }
        });
    }
    host.shutdown().await;
    Ok(())
}

#[cfg(unix)]
async fn serve_unix(host: Arc<Host>, endpoint: &Path) -> Result<()> {
    use tokio::net::UnixListener;

    if endpoint.exists() {
        let _ = std::fs::remove_file(endpoint);
    }
    let listener = UnixListener::bind(endpoint)
        .with_context(|| format!("listen on {}", endpoint.display()))?;
    loop {
        let shutdown = host.shutdown_token();
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let host = host.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, host).await {
                        tracing::warn!(
                            event = "ipc.connection_failed",
                            error = %format!("{error:#}"),
                            "IPC connection failed"
                        );
                    }
                });
            }
            _ = shutdown.cancelled() => break,
        }
    }
    let _ = std::fs::remove_file(endpoint);
    host.shutdown().await;
    Ok(())
}

async fn handle_connection<S>(stream: S, host: Arc<Host>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let connection_id = Uuid::new_v4();
    tracing::debug!(
        event = "ipc.connection_opened",
        connection_id = %connection_id,
        "IPC connection opened"
    );
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let request: RpcRequest = serde_json::from_str(&line).context("parse RPC request")?;
        let started = Instant::now();
        if request.method == "session.watch" {
            let session_id = request
                .params
                .get("session_id")
                .and_then(Value::as_str)
                .context("session_id is required")?;
            let endpoint_id = request
                .params
                .get("endpoint_id")
                .and_then(Value::as_str)
                .context("endpoint_id is required")?;
            let checkpoint_cursor = request
                .params
                .get("checkpoint_cursor")
                .and_then(Value::as_u64)
                .map(usize::try_from)
                .transpose()
                .context("checkpoint_cursor exceeds usize")?;
            match host.watch(session_id, endpoint_id, checkpoint_cursor).await {
                Ok(mut subscription) => {
                    tracing::info!(
                        event = "ipc.watch_attached",
                        connection_id = %connection_id,
                        request_id = request.id,
                        method = %request.method,
                        session_id,
                        endpoint_id,
                        duration_ms = started.elapsed().as_millis() as u64,
                        "IPC watch attached"
                    );
                    write_frame(
                        &mut write,
                        &RpcResponse {
                            id: request.id,
                            result: Some(json!({"snapshot": subscription.snapshot})),
                            error: None,
                        },
                    )
                    .await?;
                    while let Some(event) = subscription.events.recv().await {
                        tokio::time::timeout(
                            Duration::from_secs(30),
                            write_json_line(
                                &mut write,
                                &json!({"method":"session.event", "params": event}),
                            ),
                        )
                        .await
                        .context("session watch client stopped reading")??;
                    }
                    tracing::info!(
                        event = "ipc.watch_closed",
                        connection_id = %connection_id,
                        request_id = request.id,
                        method = %request.method,
                        session_id,
                        endpoint_id,
                        "IPC watch closed"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        event = "ipc.request_failed",
                        connection_id = %connection_id,
                        request_id = request.id,
                        method = %request.method,
                        session_id,
                        endpoint_id,
                        duration_ms = started.elapsed().as_millis() as u64,
                        error = %format!("{error:#}"),
                        "IPC request failed"
                    );
                    write_frame(
                        &mut write,
                        &RpcResponse {
                            id: request.id,
                            result: None,
                            error: Some(format!("{error:#}")),
                        },
                    )
                    .await?;
                }
            }
            return Ok(());
        }

        let response = match host.dispatch(&request.method, request.params).await {
            Ok(result) => RpcResponse {
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => RpcResponse {
                id: request.id,
                result: None,
                error: Some(format!("{error:#}")),
            },
        };
        if let Some(error) = response.error.as_deref() {
            tracing::warn!(
                event = "ipc.request_failed",
                connection_id = %connection_id,
                request_id = request.id,
                method = %request.method,
                duration_ms = started.elapsed().as_millis() as u64,
                error,
                "IPC request failed"
            );
        } else {
            tracing::info!(
                event = "ipc.request_completed",
                connection_id = %connection_id,
                request_id = request.id,
                method = %request.method,
                duration_ms = started.elapsed().as_millis() as u64,
                "IPC request completed"
            );
        }
        write_frame(&mut write, &response).await?;
    }
    tracing::debug!(
        event = "ipc.connection_closed",
        connection_id = %connection_id,
        "IPC connection closed"
    );
    Ok(())
}

pub async fn request(config_path: &Path, method: &str, params: Value) -> Result<Value> {
    let endpoint = endpoint(config_path);
    let request = RpcRequest {
        id: 1,
        method: method.to_string(),
        params,
    };
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let stream = ClientOptions::new()
            .open(&endpoint)
            .with_context(|| format!("connect to daemon at {endpoint}"))?;
        request_once(stream, &request).await
    }
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(&endpoint)
            .await
            .with_context(|| format!("connect to daemon at {endpoint}"))?;
        request_once(stream, &request).await
    }
}

pub async fn subscribe(
    config_path: &Path,
    session_id: &str,
    endpoint_id: &str,
) -> Result<(Value, mpsc::Receiver<Value>)> {
    let endpoint = endpoint(config_path);
    let request = RpcRequest {
        id: 1,
        method: "session.watch".to_string(),
        params: json!({"session_id": session_id, "endpoint_id": endpoint_id}),
    };
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let stream = ClientOptions::new()
            .open(&endpoint)
            .with_context(|| format!("connect to daemon at {endpoint}"))?;
        subscribe_stream(stream, &request).await
    }
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(&endpoint)
            .await
            .with_context(|| format!("connect to daemon at {endpoint}"))?;
        subscribe_stream(stream, &request).await
    }
}

async fn request_once<S>(stream: S, request: &RpcRequest) -> Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    write_json_line(&mut write, request).await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let response: RpcResponse = serde_json::from_str(&line)?;
    if let Some(error) = response.error {
        bail!(error);
    }
    Ok(response.result.unwrap_or(Value::Null))
}

async fn subscribe_stream<S>(
    stream: S,
    request: &RpcRequest,
) -> Result<(Value, mpsc::Receiver<Value>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read, mut write) = tokio::io::split(stream);
    write_json_line(&mut write, request).await?;
    let mut reader = BufReader::new(read);
    let mut first = String::new();
    if reader.read_line(&mut first).await? == 0 {
        bail!("daemon closed watch before snapshot");
    }
    let response: RpcResponse = serde_json::from_str(&first)?;
    if let Some(error) = response.error {
        bail!(error);
    }
    let snapshot = response.result.unwrap_or(Value::Null);
    let (events, receiver) = mpsc::channel(256);
    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            let Ok(length) = reader.read_line(&mut line).await else {
                break;
            };
            if length == 0 {
                break;
            }
            if let Ok(value) = serde_json::from_str(&line)
                && events.send(value).await.is_err()
            {
                break;
            }
        }
    });
    Ok((snapshot, receiver))
}

async fn write_frame<W: AsyncWrite + Unpin>(write: &mut W, response: &RpcResponse) -> Result<()> {
    write_json_line(write, response).await
}

async fn write_json_line<W: AsyncWrite + Unpin>(
    write: &mut W,
    value: &impl Serialize,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    write.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_profile(root: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root.join("resource/prompts")).unwrap();
        std::fs::write(
            root.join("resource/prompts/System.md"),
            "You are a test agent.",
        )
        .unwrap();
        let config = root.join("profile.yaml");
        std::fs::write(
            &config,
            r#"name: test
description: test agent
policyMode: confirm
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
"#,
        )
        .unwrap();
        config
    }

    #[tokio::test]
    async fn watch_writes_snapshot_before_waiting_for_events() {
        let profile = tempfile::tempdir().unwrap();
        let host = Host::load(&write_test_profile(profile.path()))
            .await
            .unwrap();
        let session = host
            .create_session(Some("watch test".to_string()), None)
            .await
            .unwrap();
        let session_id = session.id().to_string();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(handle_connection(server, host.clone()));
        let request = RpcRequest {
            id: 7,
            method: "session.watch".to_string(),
            params: json!({"session_id": session_id, "endpoint_id": "test-watch"}),
        };
        let (snapshot, events) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            subscribe_stream(client, &request),
        )
        .await
        .expect("watch must write its snapshot promptly")
        .unwrap();
        assert_eq!(snapshot["snapshot"]["record"]["info"]["id"], session_id);

        drop(events);
        server_task.abort();
        let _ = server_task.await;
        host.shutdown_token().cancel();
        host.shutdown().await;
    }
}
