//! Management-only stdio adapter. This module never creates or owns a Host.

use std::future::Future;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use dwo_protocol::{RpcError, RpcRequest, RpcResponse, RpcRoute};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::{endpoint, write_json_line};

#[cfg(windows)]
async fn connect(endpoint: &str) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    // The daemon creates the next pipe instance after accepting the previous one.
    for attempt in 0..100 {
        match ClientOptions::new().open(endpoint) {
            Ok(stream) => return Ok(stream),
            Err(error) if error.raw_os_error() == Some(231) && attempt < 99 => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error).context("connect to existing daemon management IPC"),
        }
    }
    unreachable!()
}

#[cfg(unix)]
async fn connect(endpoint: &str) -> Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(endpoint)
        .await
        .context("connect to existing daemon management IPC")
}

/// Forward JSON-line management envelopes to the existing daemon. EOF only drops
/// connections; it never sends shutdown or session cancellation requests.
pub async fn bridge_stdio(config_path: &Path) -> Result<()> {
    let endpoint = endpoint(config_path);
    let ordinary = connect(&endpoint).await?;
    bridge(tokio::io::stdin(), tokio::io::stdout(), ordinary, || {
        let endpoint = endpoint.clone();
        async move { connect(&endpoint).await }
    })
    .await
}

async fn bridge<R, W, S, C, F>(input: R, output: W, ordinary: S, connect_events: C) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Fn() -> F,
    F: Future<Output = Result<S>> + Send + 'static,
{
    let mut input = BufReader::new(input).lines();
    let (requests, mut receiver) = mpsc::unbounded_channel::<RpcRequest>();
    let (frames, mut responses) = mpsc::unbounded_channel::<Value>();
    let mut tasks = JoinSet::new();
    let ordinary_frames = frames.clone();
    let (read, mut write) = tokio::io::split(ordinary);
    // Independent read/write tasks prevent a full request pipe from blocking
    // the response that the daemon must finish writing before reading again.
    tasks.spawn(async move {
        while let Some(request) = receiver.recv().await {
            write_json_line(&mut write, &request).await?;
        }
        Ok(())
    });
    tasks.spawn(async move {
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await? {
            ordinary_frames.send(serde_json::from_str(&line)?)?;
        }
        bail!("daemon closed management IPC")
    });
    let writer = async move {
        let mut output = output;
        while let Some(frame) = responses.recv().await {
            write_json_line(&mut output, &frame).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::pin!(writer);
    let mut subscription: Option<tokio::task::AbortHandle> = None;
    loop {
        tokio::select! {
            result = &mut writer => return result,
            result = tasks.join_next() => {
                match result.context("management bridge workers stopped")? {
                    Ok(result) => result?,
                    Err(error) if error.is_cancelled() => {},
                    Err(error) => return Err(error.into()),
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                if line.trim().is_empty() { continue; }
                let request: RpcRequest = serde_json::from_str(&line).context("parse management JSON line")?;
                if request.jsonrpc != "2.0" || request.id.is_empty() || request.route != RpcRoute::Dwo {
                    frames.send(serde_json::to_value(RpcResponse::failure(
                        request.id,
                        RpcError::invalid_request("rpc requires jsonrpc 2.0, a non-empty string id and route dwo"),
                    ))?)?;
                    continue;
                }
                if request.method == "event.subscribe" {
                    // One active event subscription per stdio connection. Re-subscribe replaces it.
                    if let Some(previous) = subscription.take() { previous.abort(); }
                    let connection = connect_events();
                    let frames = frames.clone();
                    subscription = Some(tasks.spawn(async move {
                        forward_events(connection.await?, request, frames).await
                    }));
                } else {
                    requests.send(request)?;
                }
            }
        }
    }
    // JoinSet aborts its workers on every return, including stdout failure and EOF.
}

async fn forward_events<S>(
    stream: S,
    request: RpcRequest,
    frames: mpsc::UnboundedSender<Value>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    write_json_line(&mut write, &request).await?;
    let mut lines = BufReader::new(read).lines();
    let first = lines
        .next_line()
        .await?
        .context("daemon closed event IPC before replay")?;
    let response: Value = serde_json::from_str(&first)?;
    ensure!(
        response["jsonrpc"] == "2.0" && response["id"] == request.id,
        "invalid event replay response"
    );
    let failed = response.get("error").is_some();
    frames.send(response)?;
    if failed {
        return Ok(());
    }
    while let Some(line) = lines.next_line().await? {
        frames.send(serde_json::from_str(&line)?)?;
    }
    bail!("daemon closed event IPC")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    async fn frame(
        lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
    ) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn request(id: &str, method: &str) -> Value {
        json!({"jsonrpc":"2.0", "id":id, "route":"dwo", "method":method, "params":{}})
    }

    #[tokio::test]
    async fn full_duplex_backpressure_does_not_deadlock() {
        let (client, stdio) = tokio::io::duplex(8192);
        let (input, output) = tokio::io::split(stdio);
        let (ordinary, daemon) = tokio::io::duplex(64);
        let bridge = tokio::spawn(bridge(input, output, ordinary, || async {
            bail!("unexpected event connection")
        }));
        let (read, mut write) = tokio::io::split(client);
        let mut replies = BufReader::new(read).lines();
        let (read, mut daemon_write) = tokio::io::split(daemon);
        let mut requests = BufReader::new(read).lines();
        write_json_line(&mut write, &request("first", "config.snapshot"))
            .await
            .unwrap();
        assert_eq!(frame(&mut requests).await["id"], "first");
        let mut large_request = request("second", "config.update");
        large_request["params"] = json!({"text": "x".repeat(4096)});
        write_json_line(&mut write, &large_request).await.unwrap();
        // The daemon must finish a response larger than its pipe before it can
        // consume the second request, which also exceeds pipe capacity.
        let daemon = tokio::spawn(async move {
            write_json_line(
                &mut daemon_write,
                &json!({
                    "jsonrpc":"2.0", "id":"first", "result":"y".repeat(4096)
                }),
            )
            .await
            .unwrap();
            assert_eq!(frame(&mut requests).await, large_request);
            (requests, daemon_write)
        });
        assert_eq!(frame(&mut replies).await["id"], "first");
        let _daemon = tokio::time::timeout(Duration::from_secs(2), daemon)
            .await
            .unwrap()
            .unwrap();
        write.shutdown().await.unwrap();
        bridge.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stdout_failure_releases_daemon_connection() {
        let (_caller, input) = tokio::io::duplex(8192);
        let (output, display) = tokio::io::duplex(8192);
        drop(display);
        let (ordinary, daemon) = tokio::io::duplex(8192);
        let task = tokio::spawn(bridge(input, output, ordinary, || async {
            bail!("unexpected event connection")
        }));
        let (read, mut write) = tokio::io::split(daemon);
        write_json_line(
            &mut write,
            &json!({"jsonrpc":"2.0", "id":"test", "result":null}),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                BufReader::new(read).lines().next_line()
            )
            .await
            .unwrap()
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn events_use_independent_pipe_and_eof_only_drops_connections() {
        let (client, stdio) = tokio::io::duplex(8192);
        let (input, output) = tokio::io::split(stdio);
        let (ordinary, daemon) = tokio::io::duplex(8192);
        let (events, event_daemon) = tokio::io::duplex(8192);
        let events = Arc::new(Mutex::new(Some(events)));
        let task = tokio::spawn(bridge(input, output, ordinary, move || {
            let events = events
                .lock()
                .unwrap()
                .take()
                .expect("one independent event pipe");
            async move { Ok(events) }
        }));
        let (read, mut write) = tokio::io::split(client);
        let mut replies = BufReader::new(read).lines();
        let (read, mut daemon_write) = tokio::io::split(daemon);
        let mut requests = BufReader::new(read).lines();
        let (read, mut event_write) = tokio::io::split(event_daemon);
        let mut event_requests = BufReader::new(read).lines();

        write_json_line(&mut write, &request("slow", "daemon.status"))
            .await
            .unwrap();
        assert_eq!(frame(&mut requests).await, request("slow", "daemon.status"));
        // Leave the ordinary request unanswered: subscription/replay/live events must still work.
        write_json_line(&mut write, &request("sub", "event.subscribe"))
            .await
            .unwrap();
        assert_eq!(
            frame(&mut event_requests).await,
            request("sub", "event.subscribe")
        );
        let replay = json!({"jsonrpc":"2.0", "id":"sub", "result":{"events":[]}});
        let event = json!({"jsonrpc":"2.0", "route":"dwo", "method":"config.changed", "params":{"seq":1,"params":{}}});
        write_json_line(&mut event_write, &replay).await.unwrap();
        write_json_line(&mut event_write, &event).await.unwrap();
        assert_eq!(frame(&mut replies).await, replay);
        assert_eq!(frame(&mut replies).await, event);

        let error = json!({"jsonrpc":"2.0", "id":"slow", "error":{"code":"unavailable","message":"test","data":{"retry":true}}});
        write_json_line(&mut daemon_write, &error).await.unwrap();
        assert_eq!(frame(&mut replies).await, error);
        write_json_line(&mut write, &request("pending", "project.list"))
            .await
            .unwrap();
        assert_eq!(
            frame(&mut requests).await,
            request("pending", "project.list")
        );

        // EOF while both pipes are active: no daemon.shutdown/session.cancel cleanup traffic.
        write.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), requests.next_line())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(2), event_requests.next_line())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn invalid_route_is_rejected_without_reaching_daemon() {
        let (client, stdio) = tokio::io::duplex(8192);
        let (input, output) = tokio::io::split(stdio);
        let (ordinary, daemon) = tokio::io::duplex(8192);
        let task = tokio::spawn(bridge(input, output, ordinary, || async {
            bail!("unexpected event connection")
        }));
        let (read, mut write) = tokio::io::split(client);
        let mut replies = BufReader::new(read).lines();
        let mut wrong = request("wrong", "session.cancel");
        wrong["route"] = json!("acp");
        write_json_line(&mut write, &wrong).await.unwrap();
        assert_eq!(
            frame(&mut replies).await["error"]["code"],
            "invalid_request"
        );
        write.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
        assert!(
            BufReader::new(daemon)
                .lines()
                .next_line()
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn malformed_input_and_daemon_disconnect_fail_the_bridge() {
        for malformed in [true, false] {
            let (mut client, stdio) = tokio::io::duplex(8192);
            let (input, output) = tokio::io::split(stdio);
            let (ordinary, daemon) = tokio::io::duplex(8192);
            let task = tokio::spawn(bridge(input, output, ordinary, || async {
                bail!("unexpected event connection")
            }));
            if malformed {
                client.write_all(b"{broken\n").await.unwrap();
            } else {
                drop(daemon);
            }
            assert!(
                tokio::time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn absent_endpoint_returns_error_without_starting_daemon() {
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\dwo-test-{}", uuid::Uuid::new_v4());
        #[cfg(unix)]
        let endpoint = std::env::temp_dir()
            .join(format!("dwo-test-{}.sock", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        assert!(connect(&endpoint).await.is_err());
    }
}
