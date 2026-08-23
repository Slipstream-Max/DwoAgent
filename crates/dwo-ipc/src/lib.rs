use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use uuid::Uuid;

use dwo_host::Host;
use dwo_protocol::{RpcError, RpcEvent, RpcRequest, RpcResponse, RpcRoute};

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

    let security = WindowsPipeSecurity::current_owner()?;
    let mut first = true;
    loop {
        let mut options = ServerOptions::new();
        options.reject_remote_clients(true);
        if first {
            options.first_pipe_instance(true);
            first = false;
        }
        let mut attributes = security.attributes();
        // The descriptor is owned by `security` and outlives every pipe instance.
        let server = unsafe {
            options.create_with_security_attributes_raw(
                endpoint,
                (&mut attributes as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast(),
            )
        }
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

#[cfg(windows)]
struct WindowsPipeSecurity {
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
}

#[cfg(windows)]
impl WindowsPipeSecurity {
    fn current_owner() -> Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

        // Protected DACL: LocalSystem and the object owner only.
        let sddl = "D:P(A;;GA;;;SY)(A;;GA;;;OW)\0"
            .encode_utf16()
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        let result = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error()).context("build named pipe ACL");
        }
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()
                as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsPipeSecurity {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.descriptor.cast());
        }
    }
}

#[cfg(unix)]
async fn serve_unix(host: Arc<Host>, endpoint: &Path) -> Result<()> {
    use tokio::net::UnixListener;

    if endpoint.exists() {
        let _ = std::fs::remove_file(endpoint);
    }
    let listener = UnixListener::bind(endpoint)
        .with_context(|| format!("listen on {}", endpoint.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(
        endpoint,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .with_context(|| format!("restrict permissions on {}", endpoint.display()))?;
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
        if request.jsonrpc != "2.0" {
            write_frame(
                &mut write,
                &RpcResponse::failure(request.id, RpcError::invalid_request("jsonrpc must be 2.0")),
            )
            .await?;
            continue;
        }
        if !dwo_protocol::method_allowed(request.route.as_str(), &request.method) {
            write_frame(
                &mut write,
                &RpcResponse::failure(
                    request.id,
                    RpcError::invalid_request(format!(
                        "method {} is not available on the {} route",
                        request.method,
                        request.route.as_str()
                    )),
                ),
            )
            .await?;
            continue;
        }
        if request.method == "event.subscribe" {
            let cursor = request.params.get("cursor").and_then(Value::as_u64);
            let limit = request
                .params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or(50);
            let event_name = request
                .params
                .get("event")
                .and_then(Value::as_str)
                .map(str::to_string);
            let (replay, mut receiver) = host
                .subscribe_events(cursor, limit, event_name.as_deref())
                .await;
            write_frame(
                &mut write,
                &RpcResponse::success(request.id.clone(), serde_json::to_value(replay)?),
            )
            .await?;
            loop {
                match receiver.recv().await {
                    Ok(event) if event_name.as_deref().is_none_or(|name| name == event.event) => {
                        write_json_line(
                            &mut write,
                            &RpcEvent::new(
                                RpcRoute::Dwo,
                                event.event,
                                json!({"seq": event.seq, "params": event.params}),
                            ),
                        )
                        .await?;
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            return Ok(());
        }
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
                        &RpcResponse::success(
                            request.id.clone(),
                            json!({"snapshot": subscription.snapshot}),
                        ),
                    )
                    .await?;
                    while let Some(event) = subscription.events.recv().await {
                        tokio::time::timeout(
                            Duration::from_secs(30),
                            write_json_line(
                                &mut write,
                                &RpcEvent::new(
                                    request.route,
                                    "session.event",
                                    serde_json::to_value(event)?,
                                ),
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
                        &RpcResponse::failure(request.id, RpcError::from_anyhow(&error)),
                    )
                    .await?;
                }
            }
            return Ok(());
        }

        let request_id = request.id.clone();
        let response = match host
            .handle_request(
                &connection_id.to_string(),
                &request.id,
                &request.method,
                request.params,
            )
            .await
        {
            Ok(result) => RpcResponse::success(request_id.clone(), result),
            Err(error) => RpcResponse::failure(request_id.clone(), RpcError::from_anyhow(&error)),
        };
        if let Some(error) = response.error.as_ref() {
            tracing::warn!(
                event = "ipc.request_failed",
                connection_id = %connection_id,
                request_id = request_id,
                method = %request.method,
                duration_ms = started.elapsed().as_millis() as u64,
                error = %error.message,
                "IPC request failed"
            );
        } else {
            tracing::info!(
                event = "ipc.request_completed",
                connection_id = %connection_id,
                request_id = request_id,
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

pub async fn request_acp(config_path: &Path, method: &str, params: Value) -> Result<Value> {
    request_route(config_path, RpcRoute::Acp, method, params).await
}

pub async fn request_dwo(config_path: &Path, method: &str, params: Value) -> Result<Value> {
    request_route(config_path, RpcRoute::Dwo, method, params).await
}

async fn request_route(
    config_path: &Path,
    route: RpcRoute,
    method: &str,
    params: Value,
) -> Result<Value> {
    let endpoint = endpoint(config_path);
    let request = RpcRequest::new(route, method, params);
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

pub async fn subscribe_acp(
    config_path: &Path,
    session_id: &str,
    endpoint_id: &str,
) -> Result<(Value, mpsc::Receiver<Value>)> {
    let endpoint = endpoint(config_path);
    let request = RpcRequest::new(
        RpcRoute::Acp,
        "session.watch",
        json!({"session_id": session_id, "endpoint_id": endpoint_id}),
    );
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
    validate_response(&response, request)?;
    if let Some(error) = response.error {
        bail!("{}: {}", error.code, error.message);
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
    validate_response(&response, request)?;
    if let Some(error) = response.error {
        bail!("{}: {}", error.code, error.message);
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

fn validate_response(response: &RpcResponse, request: &RpcRequest) -> Result<()> {
    ensure!(
        response.jsonrpc == "2.0",
        "daemon returned an unsupported RPC version"
    );
    ensure!(
        response.id == request.id,
        "daemon returned a mismatched RPC request id"
    );
    ensure!(
        response.result.is_some() ^ response.error.is_some(),
        "daemon returned an invalid RPC response"
    );
    Ok(())
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

    #[test]
    fn rpc_envelope_is_versioned_and_routed() {
        let request = RpcRequest::new(RpcRoute::Acp, "session.list", json!({"all": true}));
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["jsonrpc"], "2.0");
        assert_eq!(encoded["route"], "acp");
        assert!(encoded["id"].as_str().is_some());
        assert!(
            serde_json::from_str::<RpcRequest>(r#"{"id":1,"method":"session.list","params":{}}"#)
                .is_err()
        );
    }

    #[test]
    fn acp_route_does_not_expose_management_methods() {
        assert!(dwo_protocol::method_allowed("acp", "session.prompt"));
        assert!(dwo_protocol::method_allowed("acp", "session.watch"));
        assert!(!dwo_protocol::method_allowed("acp", "automation.run"));
        assert!(!dwo_protocol::method_allowed("dwo", "session.prompt"));
        assert!(dwo_protocol::method_allowed("dwo", "automation.run"));
    }

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
            r#"policyMode: confirm
model:
  default:
    model: deepseek/deepseek-v4-pro
  providers:
    deepseek:
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
            jsonrpc: "2.0".to_string(),
            id: "7".to_string(),
            route: RpcRoute::Acp,
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
