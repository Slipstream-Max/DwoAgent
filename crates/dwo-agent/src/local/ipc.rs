use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::host::Host;

#[derive(Debug, Serialize, Deserialize)]
struct RpcRequest {
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct RpcResponse {
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

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
                eprintln!("IPC connection failed: {error:#}");
            }
        });
    }
    host.service.shutdown().await;
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
                        eprintln!("IPC connection failed: {error:#}");
                    }
                });
            }
            _ = shutdown.cancelled() => break,
        }
    }
    let _ = std::fs::remove_file(endpoint);
    host.service.shutdown().await;
    Ok(())
}

async fn handle_connection<S>(stream: S, host: Arc<Host>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let request: RpcRequest = serde_json::from_str(&line).context("parse RPC request")?;
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
            match host.watch(session_id, endpoint_id).await {
                Ok(mut subscription) => {
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
                        write_json_line(
                            &mut write,
                            &json!({"method":"session.event", "params": event}),
                        )
                        .await?;
                    }
                }
                Err(error) => {
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
        write_frame(&mut write, &response).await?;
    }
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
) -> Result<(Value, mpsc::UnboundedReceiver<Value>)> {
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
) -> Result<(Value, mpsc::UnboundedReceiver<Value>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read, mut write) = tokio::io::split(stream);
    write_json_line(&mut write, request).await?;
    let mut lines = BufReader::new(read).lines();
    let first = lines
        .next_line()
        .await?
        .context("daemon closed watch before snapshot")?;
    let response: RpcResponse = serde_json::from_str(&first)?;
    if let Some(error) = response.error {
        bail!(error);
    }
    let snapshot = response.result.unwrap_or(Value::Null);
    let (events, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str(&line) else {
                continue;
            };
            if events.send(value).is_err() {
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
