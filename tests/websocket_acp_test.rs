//! ACP-over-WebSocket smoke tests.

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{WebSocketStream, client_async};

const TEST_TOKEN: &str = "dwo_ws_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn websocket_initialize_and_new_session_use_acp_jsonrpc() {
    let tmp = tempfile::tempdir().unwrap();
    let port = reserve_loopback_port();
    let folder = create_websocket_agent_folder(tmp.path(), port);
    let mut server = ServerProcess::spawn(&folder);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let addr = format!("127.0.0.1:{port}");
        assert_unauthorized_connection_is_rejected(&addr, &mut server).await;
        let mut websocket = connect_with_retry(&addr, &mut server).await;

        let initialize = send_request(
            &mut websocket,
            1,
            "initialize",
            json!({"protocolVersion": 1}),
        )
        .await;
        assert_eq!(initialize["protocolVersion"], 1);
        assert_eq!(initialize["agentInfo"]["name"], "WebSocket Test Agent");
        assert_eq!(initialize["agentCapabilities"]["loadSession"], true);

        let session = send_request(
            &mut websocket,
            2,
            "session/new",
            json!({"cwd": ".", "mcpServers": []}),
        )
        .await;
        assert!(session["sessionId"].as_str().is_some());
        assert_eq!(session["modes"]["currentModeId"], "full_access");
    });
}

async fn connect_with_retry(addr: &str, server: &mut ServerProcess) -> WebSocketStream<TcpStream> {
    let url = format!("ws://{addr}");
    let mut last_error = String::new();
    for _ in 0..50 {
        if let Some(message) = server.exit_message() {
            panic!("websocket server exited before accepting connection: {message}");
        }

        match TcpStream::connect(addr).await {
            Ok(stream) => match client_async(authorized_request(&url), stream).await {
                Ok((websocket, _response)) => return websocket,
                Err(err) => last_error = err.to_string(),
            },
            Err(err) => last_error = err.to_string(),
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out connecting to websocket server at {url}: {last_error}");
}

async fn assert_unauthorized_connection_is_rejected(addr: &str, server: &mut ServerProcess) {
    let url = format!("ws://{addr}");
    for _ in 0..50 {
        if let Some(message) = server.exit_message() {
            panic!("websocket server exited before unauthorized check: {message}");
        }

        if let Ok(stream) = TcpStream::connect(addr).await {
            let result = client_async(url.as_str(), stream).await;
            assert!(
                result.is_err(),
                "unauthorized websocket connection should fail"
            );
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out connecting to websocket server for unauthorized check");
}

fn authorized_request(url: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {TEST_TOKEN}")).unwrap(),
    );
    request
}

async fn send_request(
    websocket: &mut WebSocketStream<TcpStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    websocket
        .send(Message::Text(request.to_string()))
        .await
        .unwrap();

    let response = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = websocket
                .next()
                .await
                .expect("websocket closed before response")
                .expect("read websocket response");
            let value = match message {
                Message::Text(text) => serde_json::from_str::<Value>(&text).unwrap(),
                Message::Binary(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
                Message::Close(close) => panic!("websocket closed before response: {close:?}"),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    })
    .await
    .expect("timed out waiting for websocket ACP response");

    if let Some(error) = response.get("error") {
        panic!("ACP error for {method}: {error}");
    }
    response.get("result").cloned().unwrap_or(Value::Null)
}

fn reserve_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn create_websocket_agent_folder(tmp_dir: &Path, port: u16) -> PathBuf {
    let agent_dir = tmp_dir.join("agent");
    let prompt_dir = agent_dir.join("resources").join("prompt");
    std::fs::create_dir_all(&prompt_dir).unwrap();

    std::fs::write(
        agent_dir.join("agent.yaml"),
        format!(
            "\
agent_id: websocket-test-agent
name: WebSocket Test Agent
description: Test agent for websocket ACP ingress
max_running_turn: 5
policy_mode: full_access
model:
  default_model_id: mock-model
  models:
    - model_name: mock-model
      provider: deepseek
      model_id: deepseek-v4-pro
      api_key: test-key-not-real
      api_base: http://127.0.0.1:9/v1
      default_reasoning_mode: auto
      compact_threshold: 0.8
channels:
  websocket:
    enabled: true
    bind_addr: 127.0.0.1:{port}
    auth: true
"
        ),
    )
    .unwrap();
    let secret_dir = agent_dir
        .join("runtime")
        .join("channel_secret")
        .join("websocket");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::write(
        secret_dir.join("auth.yaml"),
        format!("token: \"{TEST_TOKEN}\"\n"),
    )
    .unwrap();

    std::fs::write(
        prompt_dir.join("system.md"),
        "You are a websocket test agent.\n",
    )
    .unwrap();
    agent_dir
}

struct ServerProcess {
    child: Child,
}

impl ServerProcess {
    fn spawn(agent_folder: &Path) -> Self {
        let binary = agent_binary_path();
        let child = Command::new(&binary)
            .args(["serve", "--agent-folder", agent_folder.to_str().unwrap()])
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {}: {err}", binary.display()));
        Self { child }
    }

    fn exit_message(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok().flatten()?;
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        Some(format!("{status}; stderr: {stderr}"))
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn agent_binary_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_dwo-agent") {
        return PathBuf::from(path);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = if cfg!(windows) {
        "dwo-agent.exe"
    } else {
        "dwo-agent"
    };
    for profile in ["debug", "release"] {
        let path = manifest_dir.join("target").join(profile).join(binary);
        if path.exists() {
            return path;
        }
    }
    panic!("Cannot find dwo-agent binary. Run `cargo build` first.");
}
