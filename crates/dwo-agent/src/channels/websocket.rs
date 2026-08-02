use std::sync::Arc;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::{StatusCode, Uri};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;

use crate::host::Host;
use crate::local::acp;

pub(crate) struct RunningWebsocket {
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl RunningWebsocket {
    pub(crate) async fn start(host: Arc<Host>) -> Result<Self> {
        let runtime = host.channels().load_websocket().await?;
        let listener = TcpListener::bind(("0.0.0.0", runtime.config.port))
            .await
            .with_context(|| format!("listen on 0.0.0.0:{}", runtime.config.port))?;
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let config_path = host.config_path().to_path_buf();
        let token = Arc::new(runtime.token);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let token = token.clone();
                        let config_path = config_path.clone();
                        let connection_shutdown = task_shutdown.clone();
                        tokio::spawn(async move {
                            let _ =
                                serve_connection(stream, config_path, token, connection_shutdown)
                                    .await;
                        });
                    }
                    _ = task_shutdown.cancelled() => break,
                }
            }
        });
        Ok(Self { shutdown, task })
    }

    pub(crate) async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

async fn serve_connection(
    stream: TcpStream,
    config_path: std::path::PathBuf,
    token: Arc<String>,
    shutdown: CancellationToken,
) -> Result<()> {
    let websocket = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, mut response: Response| {
            let path_ok = request.uri().path() == "/acp";
            let token_ok = token_matches(request.uri(), &token);
            if path_ok && token_ok {
                Ok(response)
            } else {
                *response.status_mut() = if path_ok {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::NOT_FOUND
                };
                Err(response.map(|_| Some("WebSocket authentication failed".to_string())))
            }
        },
    )
    .await
    .context("WebSocket handshake")?;

    let (acp_stream, bridge_stream) = tokio::io::duplex(1024 * 1024);
    let (acp_read, acp_write) = tokio::io::split(acp_stream);
    let acp_task = tokio::spawn(acp::run_with_io(config_path, acp_read, acp_write));
    let (bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
    let mut outbound = BufReader::new(bridge_read).lines();
    let (mut socket_write, mut socket_read) = websocket.split();

    loop {
        tokio::select! {
            incoming = socket_read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let message: serde_json::Value =
                        serde_json::from_str(&text).context("parse ACP WebSocket frame")?;
                    let message = serde_json::to_vec(&message)?;
                    bridge_write.write_all(&message).await?;
                    bridge_write.write_all(b"\n").await?;
                    bridge_write.flush().await?;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Binary(_))) => {
                    socket_write.send(Message::Close(None)).await?;
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
            },
            line = outbound.next_line() => match line? {
                Some(line) => socket_write.send(Message::Text(line.into())).await?,
                None => break,
            },
            _ = shutdown.cancelled() => {
                let _ = socket_write.send(Message::Close(None)).await;
                break;
            }
        }
    }
    bridge_write.shutdown().await?;
    drop(bridge_write);
    let _ = acp_task.await;
    Ok(())
}

fn token_matches(uri: &Uri, expected: &str) -> bool {
    let supplied = uri
        .query()
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "token")
                .map(|(_, value)| value.into_owned())
        })
        .unwrap_or_default();
    supplied.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_query_is_url_decoded_and_compared() {
        let uri: Uri = "/acp?other=value&token=a%2Fb_c".parse().unwrap();
        assert!(token_matches(&uri, "a/b_c"));
        assert!(!token_matches(&uri, "a/b_d"));
    }

    #[tokio::test]
    async fn websocket_runs_acp_and_rejects_the_wrong_token() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let config_path = profile.path().join("profile.yaml");
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let config_path = config_path.clone();
                let shutdown = server_shutdown.clone();
                tokio::spawn(async move {
                    let _ =
                        serve_connection(stream, config_path, Arc::new("secret".into()), shutdown)
                            .await;
                });
            }
        });

        let wrong = tokio_tungstenite::connect_async(format!("ws://{address}/acp?token=wrong"))
            .await
            .unwrap_err();
        assert!(wrong.to_string().contains("401"));

        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/acp?token=secret"))
                .await
                .unwrap();
        socket
            .send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {"protocolVersion": 1}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let response = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], 1);

        socket.close(None).await.unwrap();
        shutdown.cancel();
        server.await.unwrap();
    }
}
