use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use dwo_host::{Host, HostEvent};
use dwo_protocol::{RpcError, RpcEvent, RpcRequest, RpcResponse, RpcRoute};

const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;

/// Runs the optional remote transport until the Host shuts down.
///
/// The listener is owned here rather than by a channel. Host configuration
/// events are the only control plane needed to stop and recreate it.
pub async fn serve(host: Arc<Host>) -> Result<()> {
    let (_replay, mut events) = host.subscribe_events(None, 1, None).await;
    let shutdown = host.shutdown_token();
    let mut running: Option<RunningWebsocket> = None;
    let mut health_check = tokio::time::interval(std::time::Duration::from_secs(5));

    reconcile(&host, &mut running).await;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            event = events.recv() => {
                match event {
                    Ok(HostEvent { event, .. }) if matches!(event.as_str(), "websocket.status" | "config.changed") => {
                        reconcile(&host, &mut running).await;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        reconcile(&host, &mut running).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = health_check.tick() => {
                let should_reconcile = running
                    .as_ref()
                    .is_some_and(RunningWebsocket::is_finished)
                    || (running.is_none() && host.websocket_snapshot().enabled);
                if should_reconcile {
                    reconcile(&host, &mut running).await;
                }
            }
        }
    }

    if let Some(listener) = running.take() {
        listener.stop().await;
    }
    host.set_websocket_running(false);
    Ok(())
}

async fn reconcile(host: &Arc<Host>, running: &mut Option<RunningWebsocket>) {
    let runtime = match host.websocket_runtime().await {
        Ok(runtime) => runtime,
        Err(error) => {
            host.set_websocket_running(false);
            tracing::warn!(event = "websocket.config_failed", error = %format!("{error:#}"), "cannot load WebSocket runtime");
            return;
        }
    };

    if running.as_ref().is_some_and(|listener| {
        runtime.config.enabled && !listener.is_finished() && listener.matches(&runtime)
    }) {
        return;
    }

    if let Some(listener) = running.take() {
        listener.stop().await;
    }
    host.set_websocket_running(false);

    if !runtime.config.enabled {
        return;
    }

    match RunningWebsocket::start(host.clone(), runtime).await {
        Ok(listener) => {
            host.set_websocket_running(true);
            *running = Some(listener);
        }
        Err(error) => {
            tracing::warn!(event = "websocket.listen_failed", error = %format!("{error:#}"), "WebSocket transport is unavailable");
        }
    }
}

struct RunningWebsocket {
    bind: String,
    port: u16,
    acp_token: String,
    management_token: String,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl RunningWebsocket {
    async fn start(host: Arc<Host>, runtime: dwo_host::WebsocketRuntime) -> Result<Self> {
        let listener = TcpListener::bind((&*runtime.config.bind, runtime.config.port))
            .await
            .with_context(|| {
                format!("listen on {}:{}", runtime.config.bind, runtime.config.port)
            })?;
        let bind = runtime.config.bind.clone();
        let port = runtime.config.port;
        let acp_token_value = runtime.acp_token.clone();
        let management_token_value = runtime.management_token.clone();
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let acp_token = Arc::new(runtime.acp_token);
        let management_token = Arc::new(runtime.management_token);
        let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else {
                            tracing::warn!(event = "websocket.accept_failed", "WebSocket listener stopped");
                            break;
                        };
                        let acp_token = acp_token.clone();
                        let management_token = management_token.clone();
                        let connections = connections.clone();
                        let host = host.clone();
                        let connection_shutdown = task_shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(error) = serve_connection(
                                stream,
                                host,
                                acp_token,
                                management_token,
                                connections,
                                connection_shutdown,
                            ).await {
                                tracing::debug!(event = "websocket.connection_failed", error = %format!("{error:#}"), "WebSocket connection ended");
                            }
                        });
                    }
                    _ = task_shutdown.cancelled() => break,
                }
            }
            host.set_websocket_running(false);
        });
        Ok(Self {
            bind,
            port,
            acp_token: acp_token_value,
            management_token: management_token_value,
            shutdown,
            task,
        })
    }

    fn matches(&self, runtime: &dwo_host::WebsocketRuntime) -> bool {
        self.bind == runtime.config.bind
            && self.port == runtime.config.port
            && self.acp_token == runtime.acp_token
            && self.management_token == runtime.management_token
    }

    fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

#[allow(clippy::result_large_err)]
async fn serve_connection(
    stream: TcpStream,
    host: Arc<Host>,
    acp_token: Arc<String>,
    management_token: Arc<String>,
    connections: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()> {
    let _permit = connections
        .acquire_owned()
        .await
        .context("WebSocket connection limit reached")?;
    let requested_path = Arc::new(AtomicU8::new(0));
    let path_capture = requested_path.clone();
    let websocket = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, mut response: Response| {
            let path = request.uri().path();
            let route = if path == "/dwo" {
                2
            } else if path == "/acp" {
                1
            } else {
                0
            };
            path_capture.store(route, Ordering::Release);
            let expected = if route == 2 {
                &management_token
            } else {
                &acp_token
            };
            if route != 0 && token_matches(request, expected) {
                Ok(response)
            } else {
                *response.status_mut() = if route == 0 {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::UNAUTHORIZED
                };
                Err(response.map(|_| Some("WebSocket authentication failed".to_string())))
            }
        },
    )
    .await
    .context("WebSocket handshake")?;

    match requested_path.load(Ordering::Acquire) {
        1 => serve_acp_connection(websocket, host, shutdown).await,
        2 => serve_dwo_connection(websocket, host, shutdown).await,
        _ => unreachable!("handshake accepted without a route"),
    }
}

async fn serve_acp_connection(
    websocket: WebSocketStream<TcpStream>,
    host: Arc<Host>,
    shutdown: CancellationToken,
) -> Result<()> {
    let (acp_stream, bridge_stream) = tokio::io::duplex(1024 * 1024);
    let (acp_read, acp_write) = tokio::io::split(acp_stream);
    let acp_task = tokio::spawn(dwo_acp::run_with_host_io(host, acp_read, acp_write));
    let (bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
    let mut outbound = BufReader::new(bridge_read).lines();
    let (mut socket_write, mut socket_read) = websocket.split();

    loop {
        tokio::select! {
            incoming = socket_read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if text.len() > MAX_MESSAGE_BYTES { socket_write.send(Message::Close(None)).await?; break; }
                    let value: serde_json::Value = serde_json::from_str(&text).context("parse ACP WebSocket frame")?;
                    bridge_write.write_all(serde_json::to_string(&value)?.as_bytes()).await?;
                    bridge_write.write_all(b"\n").await?;
                    bridge_write.flush().await?;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Binary(_))) => { socket_write.send(Message::Close(None)).await?; break; }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
            },
            line = outbound.next_line() => match line? {
                Some(line) => socket_write.send(Message::Text(line.into())).await?,
                None => break,
            },
            _ = shutdown.cancelled() => { let _ = socket_write.send(Message::Close(None)).await; break; },
        }
    }
    bridge_write.shutdown().await?;
    drop(bridge_write);
    let _ = acp_task.await;
    Ok(())
}

async fn serve_dwo_connection(
    websocket: WebSocketStream<TcpStream>,
    host: Arc<Host>,
    shutdown: CancellationToken,
) -> Result<()> {
    let client_id = format!("websocket-{}", Uuid::new_v4());
    let (mut socket_write, mut socket_read) = websocket.split();
    let mut event_receiver: Option<tokio::sync::broadcast::Receiver<HostEvent>> = None;
    let mut event_name: Option<String> = None;
    loop {
        tokio::select! {
            event = async {
                match event_receiver.as_mut() {
                    Some(receiver) => receiver.recv().await.ok(),
                    None => std::future::pending().await,
                }
            }, if event_receiver.is_some() => {
                if let Some(event) = event
                    && event_name.as_deref().is_none_or(|name| name == event.event)
                {
                    socket_write.send(Message::Text(serde_json::to_string(&RpcEvent::new(
                        RpcRoute::Dwo,
                        event.event,
                        serde_json::json!({"seq": event.seq, "params": event.params}),
                    ))?.into())).await?;
                }
            },
            incoming = socket_read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if text.len() > MAX_MESSAGE_BYTES { socket_write.send(Message::Close(None)).await?; break; }
                    let request: RpcRequest = match serde_json::from_str(&text) {
                        Ok(request) => request,
                        Err(error) => {
                            let response = RpcResponse::failure("".to_string(), RpcError::invalid_request(format!("parse Dwo RPC request: {error}")));
                            socket_write.send(Message::Text(serde_json::to_string(&response)?.into())).await?;
                            continue;
                        }
                    };
                    if request.jsonrpc == "2.0" && request.route == RpcRoute::Dwo && request.method == "event.subscribe" {
                        let cursor = request.params.get("cursor").and_then(serde_json::Value::as_u64);
                        let limit = request.params.get("limit").and_then(serde_json::Value::as_u64).unwrap_or(50) as usize;
                        let requested_event = request.params.get("event").and_then(serde_json::Value::as_str).map(str::to_string);
                        let (replay, receiver) = host.subscribe_events(cursor, limit, requested_event.as_deref()).await;
                        event_receiver = Some(receiver);
                        event_name = requested_event;
                        socket_write.send(Message::Text(serde_json::to_string(&RpcResponse::success(request.id, serde_json::to_value(replay)?))?.into())).await?;
                        continue;
                    }
                    let response = if request.jsonrpc != "2.0" {
                        RpcResponse::failure(request.id, RpcError::invalid_request("jsonrpc must be 2.0"))
                    } else if request.route != RpcRoute::Dwo {
                        RpcResponse::failure(request.id, RpcError::invalid_request("WebSocket /dwo only accepts route=dwo"))
                    } else if !dwo_protocol::method_allowed(request.route.as_str(), &request.method) {
                        RpcResponse::failure(request.id, RpcError::invalid_request(format!("method {} is not available on the dwo route", request.method)))
                    } else {
                        match host.handle_request(&client_id, &request.id, &request.method, request.params).await {
                            Ok(result) => RpcResponse::success(request.id, result),
                            Err(error) => RpcResponse::failure(request.id, RpcError::from_anyhow(&error)),
                        }
                    };
                    socket_write.send(Message::Text(serde_json::to_string(&response)?.into())).await?;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Binary(_))) => { socket_write.send(Message::Close(None)).await?; break; }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
            },
            _ = shutdown.cancelled() => { let _ = socket_write.send(Message::Close(None)).await; break; },
        }
    }
    Ok(())
}

fn token_matches(request: &Request, expected: &str) -> bool {
    let header_token = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let supplied = header_token
        .map(str::to_string)
        .or_else(|| {
            request.uri().query().and_then(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(key, _)| key == "token")
                    .map(|(_, value)| value.into_owned())
            })
        })
        .unwrap_or_default();
    supplied.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_query_is_decoded_and_compared_constant_time() {
        let request = Request::builder()
            .uri("/acp?token=a%2Fb_c")
            .body(())
            .unwrap();
        assert!(token_matches(&request, "a/b_c"));
        assert!(!token_matches(&request, "a/b_d"));
    }

    #[test]
    fn bearer_token_has_precedence() {
        let request = Request::builder()
            .uri("/dwo?token=wrong")
            .header("authorization", "Bearer right")
            .body(())
            .unwrap();
        assert!(token_matches(&request, "right"));
    }

    #[tokio::test]
    async fn listener_routes_auth_hot_rebinds_and_stops_without_races() {
        let port = reserve_port().await;

        let profile = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(profile.path().join("resource/prompts")).unwrap();
        std::fs::write(
            profile.path().join("resource/prompts/System.md"),
            "You are a test agent.",
        )
        .unwrap();
        let config_path = profile.path().join("profile.yaml");
        std::fs::write(
            &config_path,
            format!(
                r#"policyMode: confirm
websocket:
  enabled: true
  bind: 127.0.0.1
  port: {port}
model:
  default:
    model: deepseek/deepseek-v4-pro
  providers:
    deepseek:
"#
            ),
        )
        .unwrap();

        let host = Host::load(&config_path).await.unwrap();
        let server = tokio::spawn(serve(host.clone()));
        wait_for_running(&host, true).await;
        let tokens = host.websocket_token().await.unwrap();
        let acp_token = tokens["acpToken"].as_str().unwrap();
        let management_token = tokens["managementToken"].as_str().unwrap();
        let address = format!("127.0.0.1:{port}");

        let wrong =
            tokio_tungstenite::connect_async(format!("ws://{address}/dwo?token={acp_token}"))
                .await
                .unwrap_err();
        assert!(wrong.to_string().contains("401"));

        let (mut acp, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/acp?token={acp_token}"))
                .await
                .unwrap();
        acp.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 2,
                    "info": {"name": "websocket-test", "version": "0.1.0"},
                    "capabilities": {}
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        let response = acp.next().await.unwrap().unwrap().into_text().unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["result"]["protocolVersion"], 2);
        acp.close(None).await.unwrap();

        let (mut management, _) = tokio_tungstenite::connect_async(format!(
            "ws://{address}/dwo?token={management_token}"
        ))
        .await
        .unwrap();
        management
            .send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": "status",
                    "route": "dwo",
                    "method": "daemon.status",
                    "params": {}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let response = management
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["result"]["healthy"], true);
        management.close(None).await.unwrap();

        let replacement_port = reserve_port().await;
        host.websocket_config(Some(json!({
            "enabled": true,
            "bind": "127.0.0.1",
            "port": replacement_port,
        })))
        .await
        .unwrap();
        let replacement_address = format!("127.0.0.1:{replacement_port}");
        let mut replacement = wait_for_connection(format!(
            "ws://{replacement_address}/dwo?token={management_token}"
        ))
        .await;
        replacement.close(None).await.unwrap();
        assert!(
            tokio_tungstenite::connect_async(format!(
                "ws://{address}/dwo?token={management_token}"
            ))
            .await
            .is_err(),
            "old listener remained reachable after hot rebind"
        );

        host.websocket_set_enabled(false).await.unwrap();
        wait_for_running(&host, false).await;
        assert!(
            tokio_tungstenite::connect_async(format!(
                "ws://{replacement_address}/acp?token={acp_token}"
            ))
            .await
            .is_err()
        );

        host.shutdown_token().cancel();
        server.await.unwrap().unwrap();
        host.shutdown().await;
    }

    async fn reserve_port() -> u16 {
        let reservation = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        reservation.local_addr().unwrap().port()
    }

    async fn wait_for_connection(
        endpoint: String,
    ) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
        for _ in 0..100 {
            if let Ok((socket, _)) = tokio_tungstenite::connect_async(&endpoint).await {
                return socket;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("WebSocket listener did not accept connections at {endpoint}");
    }

    async fn wait_for_running(host: &Arc<Host>, expected: bool) {
        for _ in 0..100 {
            let status = host.websocket_status().await.unwrap();
            if status["running"] == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("WebSocket running state did not become {expected}");
    }
}
