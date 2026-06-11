//! ACP protocol-level conformance tests for the Rust agent.
//!
//! Run with: `cargo test --test acp_protocol_test`
//! Requires: `cargo build` first (needs the dwo-agent binary).
//!
//! Coverage:
//! - initialize: capabilities negotiation
//! - session/new: returns session id, modes, config options
//! - session/list: returns created sessions
//! - session/prompt: returns end_turn, emits agent_message_chunk notifications
//! - session/cancel: cancels in-flight model reply / running tool
//! - prompt preemption: new prompt while running cancels old and runs new
//! - invalid request rejection: empty prompt, bad session id, unsupported types

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

// ── Mock LLM Server (fast) ─────────────────────────────────────────────────

struct MockLlmServer {
    _handle: thread::JoinHandle<()>,
    port: u16,
    request_count: Arc<AtomicUsize>,
}

impl MockLlmServer {
    fn start() -> Self {
        Self::start_with_delay(Duration::ZERO)
    }

    /// Start a mock LLM that delays `delay` before responding.
    /// Useful for testing cancel/preemption.
    fn start_with_delay(delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock LLM");
        let port = listener.local_addr().unwrap().port();
        let request_count = Arc::new(AtomicUsize::new(0));
        let rc = request_count.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let rc = rc.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut content_length: usize = 0;
                    let mut line = String::new();
                    reader.read_line(&mut line).ok();
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).ok();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = header.to_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    std::io::Read::read_exact(&mut reader, &mut body).ok();
                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let is_stream = request
                        .get("stream")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);

                    rc.fetch_add(1, Ordering::SeqCst);

                    if !delay.is_zero() {
                        thread::sleep(delay);
                    }

                    if is_stream {
                        let chunks = vec![
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}),
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello from mock LLM."}}]}),
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
                        ];
                        let mut sse = String::new();
                        for c in &chunks {
                            sse.push_str(&format!(
                                "data: {}\n\n",
                                serde_json::to_string(c).unwrap()
                            ));
                        }
                        sse.push_str("data: [DONE]\n\n");
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n{sse}"
                        );
                        stream.write_all(resp.as_bytes()).ok();
                    } else {
                        let body = json!({"id":"m1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"Hello from mock LLM."},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}});
                        let payload = serde_json::to_string(&body).unwrap();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
                            payload.len()
                        );
                        stream.write_all(resp.as_bytes()).ok();
                    }
                    stream.flush().ok();
                });
            }
        });
        thread::sleep(Duration::from_millis(50));
        Self {
            _handle: handle,
            port,
            request_count,
        }
    }
}

// ── Mock LLM Server (slow streaming, for cancel tests) ─────────────────────

struct ToolCallingLlm {
    _handle: thread::JoinHandle<()>,
    port: u16,
    request_count: Arc<AtomicUsize>,
}

impl ToolCallingLlm {
    fn start_with_first_response_delay(delay: Duration, patch: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind tool-calling LLM");
        let port = listener.local_addr().unwrap().port();
        let request_count = Arc::new(AtomicUsize::new(0));
        let rc = request_count.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let rc = rc.clone();
                let patch = patch.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut content_length: usize = 0;
                    let mut line = String::new();
                    reader.read_line(&mut line).ok();
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).ok();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = header.to_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    std::io::Read::read_exact(&mut reader, &mut body).ok();
                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let is_stream = request
                        .get("stream")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let request_index = rc.fetch_add(1, Ordering::SeqCst);

                    if request_index == 0 {
                        thread::sleep(delay);
                    }

                    if !is_stream {
                        let body = json!({"id":"m1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}});
                        let payload = serde_json::to_string(&body).unwrap();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
                            payload.len()
                        );
                        stream.write_all(resp.as_bytes()).ok();
                        stream.flush().ok();
                        return;
                    }

                    let chunks = if request_index == 0 {
                        let args = serde_json::to_string(&json!({"patch": patch})).unwrap();
                        vec![
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}),
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_file_edit","type":"function","function":{"name":"file_edit","arguments":args}}]}}]}),
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
                        ]
                    } else {
                        vec![
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}),
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Done."}}]}),
                            json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
                        ]
                    };
                    let mut sse = String::new();
                    for c in &chunks {
                        sse.push_str(&format!("data: {}\n\n", serde_json::to_string(c).unwrap()));
                    }
                    sse.push_str("data: [DONE]\n\n");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n{sse}"
                    );
                    stream.write_all(resp.as_bytes()).ok();
                    stream.flush().ok();
                });
            }
        });
        thread::sleep(Duration::from_millis(50));
        Self {
            _handle: handle,
            port,
            request_count,
        }
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

struct SlowStreamingLlm {
    _handle: thread::JoinHandle<()>,
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
    #[allow(dead_code)]
    cancelled: Arc<AtomicBool>,
}

impl SlowStreamingLlm {
    /// Streams chunks with a long delay between them.
    /// When the client disconnects (cancel), the write will fail.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow LLM");
        let port = listener.local_addr().unwrap().port();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_clone = cancelled.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_clone = requests.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let cancelled_inner = cancelled_clone.clone();
                let requests_inner = requests_clone.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut content_length: usize = 0;
                    let mut line = String::new();
                    reader.read_line(&mut line).ok();
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).ok();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = header.to_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    std::io::Read::read_exact(&mut reader, &mut body).ok();
                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    requests_inner.lock().unwrap().push(request);

                    // Send headers for SSE.
                    let header_resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n";
                    if stream.write_all(header_resp.as_bytes()).is_err() {
                        return;
                    }
                    stream.flush().ok();

                    // First chunk: role.
                    let c1 = json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]});
                    let chunk1 = format!("data: {}\n\n", serde_json::to_string(&c1).unwrap());
                    if stream.write_all(chunk1.as_bytes()).is_err() {
                        cancelled_inner.store(true, Ordering::SeqCst);
                        return;
                    }
                    stream.flush().ok();

                    // Long delay — this is where cancel should interrupt.
                    thread::sleep(Duration::from_secs(10));

                    // If we get here, cancel didn't work.
                    let c2 = json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Should not see this."}}]});
                    let c3 = json!({"id":"m1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}});
                    let rest = format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        serde_json::to_string(&c2).unwrap(),
                        serde_json::to_string(&c3).unwrap()
                    );
                    if stream.write_all(rest.as_bytes()).is_err() {
                        cancelled_inner.store(true, Ordering::SeqCst);
                    }
                    stream.flush().ok();
                });
            }
        });
        thread::sleep(Duration::from_millis(50));
        Self {
            _handle: handle,
            port,
            requests,
            cancelled,
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

// ── Agent Folder Setup ─────────────────────────────────────────────────────

fn create_mock_agent_folder(tmp_dir: &Path, llm_port: u16) -> PathBuf {
    let agent_dir = tmp_dir.join("agent");
    let resources_dir = agent_dir.join("resources");
    let agents_dir = resources_dir.join("agents");
    let skills_dir = resources_dir.join("skills");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::create_dir_all(&skills_dir).unwrap();

    std::fs::write(
        agent_dir.join("agent.yaml"),
        "\
agent_id: test-agent
name: Test Agent
description: Test agent for protocol conformance
max_running_turn: 5
policy_mode: full_access
session_store_dir: .sessions
",
    )
    .unwrap();

    std::fs::write(
        agent_dir.join("model.yaml"),
        format!(
            "\
default_model_id: mock-model
models:
  - model_name: mock-model
    provider: deepseek
    model_id: deepseek-v4-pro
    api_key: test-key-not-real
    api_base: http://127.0.0.1:{llm_port}/v1
    default_reasoning_mode: auto
    compact_threshold: 0.8
  - model_name: mock-flash
    provider: deepseek
    model_id: deepseek-v4-flash
    api_key: test-key-not-real
    api_base: http://127.0.0.1:{llm_port}/v1
    default_reasoning_mode: auto
    compact_threshold: 0.8
"
        ),
    )
    .unwrap();

    std::fs::write(
        agents_dir.join("test-agent.agent.md"),
        "You are a test agent.\n",
    )
    .unwrap();
    agent_dir
}

// ── ACP Client ─────────────────────────────────────────────────────────────

struct AcpClient {
    stdin: std::process::ChildStdin,
    responses: std::sync::mpsc::Receiver<Value>,
    notifications: Arc<Mutex<Vec<Value>>>,
    request_id: AtomicU64,
    child: Child,
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // Ensure the child process is killed when the test ends.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl AcpClient {
    fn spawn(agent_folder: &Path) -> Self {
        let binary = find_agent_binary();
        let mut child = Command::new(&binary)
            .args(["acp", "--agent-folder", agent_folder.to_str().unwrap()])
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to spawn {}: {e}", binary.display()));

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let notifications = Arc::new(Mutex::new(Vec::<Value>::new()));
        let notif_clone = notifications.clone();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if msg.get("id").is_some() && msg.get("id") != Some(&Value::Null) {
                    let _ = tx.send(msg);
                } else {
                    notif_clone.lock().unwrap().push(msg);
                }
            }
        });

        // Wait for agent to start.
        thread::sleep(Duration::from_millis(500));
        if let Some(status) = child.try_wait().ok().flatten() {
            let mut stderr_buf = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                stderr.read_to_string(&mut stderr_buf).ok();
            }
            panic!("Agent exited with {status}.\nstderr: {stderr_buf}");
        }

        Self {
            stdin,
            responses: rx,
            notifications,
            request_id: AtomicU64::new(0),
            child,
        }
    }

    /// Send a request and expect a successful result (panics on error).
    fn send_request(&mut self, method: &str, params: Value) -> Value {
        let resp = self.send_request_raw(method, params);
        if let Some(err) = resp.get("error") {
            panic!("ACP error for {method}: {err}");
        }
        resp.get("result").cloned().unwrap_or(Value::Null)
    }

    /// Send a request and return the full JSON-RPC response (including error).
    fn send_request_raw(&mut self, method: &str, params: Value) -> Value {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst) + 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            match self.responses.recv_timeout(Duration::from_millis(100)) {
                Ok(resp) if resp.get("id").and_then(Value::as_u64) == Some(id) => {
                    return resp;
                }
                Ok(_) => {} // not our response
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() > deadline {
                        panic!("Timeout waiting for response to {method} (id={id})");
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("Agent stdout closed while waiting for {method}");
                }
            }
        }
    }

    /// Send a request without waiting for a response (fire-and-forget request).
    fn send_request_no_wait(&mut self, method: &str, params: Value) -> u64 {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst) + 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();
        id
    }

    /// Wait for a specific request id response with custom timeout.
    fn wait_response(&mut self, id: u64, timeout: Duration) -> Option<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.responses.recv_timeout(Duration::from_millis(100)) {
                Ok(resp) if resp.get("id").and_then(Value::as_u64) == Some(id) => {
                    return Some(resp);
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() > deadline {
                        return None;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return None;
                }
            }
        }
    }

    fn send_notification(&mut self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        writeln!(self.stdin, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn drain_notifications(&self) -> Vec<Value> {
        thread::sleep(Duration::from_millis(300));
        std::mem::take(&mut *self.notifications.lock().unwrap())
    }

    fn clear_notifications(&self) {
        self.notifications.lock().unwrap().clear();
    }
}

fn config_current_value<'a>(result: &'a Value, id: &str) -> &'a Value {
    result["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == id)
        .map(|option| &option["currentValue"])
        .unwrap()
}

fn usage_update(notification: &Value) -> Option<&Value> {
    let update = notification.pointer("/params/update")?;
    (update.get("sessionUpdate").and_then(Value::as_str) == Some("usage_update")).then_some(update)
}

fn read_utf8_bom_text(path: impl AsRef<Path>) -> String {
    const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
    let bytes = std::fs::read(path.as_ref()).unwrap();
    assert!(
        bytes.starts_with(UTF8_BOM),
        "{} should be UTF-8 with BOM",
        path.as_ref().display()
    );
    std::str::from_utf8(&bytes[UTF8_BOM.len()..])
        .unwrap()
        .to_string()
}

fn find_agent_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for sub in ["debug", "release"] {
        let p = manifest_dir
            .join("target")
            .join(sub)
            .join(if cfg!(windows) {
                "dwo-agent.exe"
            } else {
                "dwo-agent"
            });
        if p.exists() {
            return p;
        }
    }
    panic!("Cannot find dwo-agent binary. Run `cargo build` first.");
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests: Basic Protocol
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_initialize() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);

    let r = c.send_request("initialize", json!({"protocolVersion": 1}));
    assert_eq!(r["protocolVersion"], 1);
    assert_eq!(r["agentInfo"]["name"], "Test Agent");
    assert_eq!(r["agentCapabilities"]["loadSession"], true);
    assert_eq!(r["agentCapabilities"]["promptCapabilities"]["image"], true);
    assert_eq!(r["agentCapabilities"]["promptCapabilities"]["audio"], false);
    assert_eq!(
        r["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
        true
    );
}

#[test]
fn test_stdio_eof_exits_process() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let binary = find_agent_binary();
    let mut child = Command::new(&binary)
        .args(["acp", "--agent-folder", folder.to_str().unwrap()])
        .env("NO_PROXY", "127.0.0.1,localhost,::1")
        .env("no_proxy", "127.0.0.1,localhost,::1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn {}: {e}", binary.display()));

    thread::sleep(Duration::from_millis(500));
    let stdin = child.stdin.take().unwrap();
    drop(stdin);

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "process exited with {status}");
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("agent process did not exit after stdin EOF");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn test_new_session_returns_config_and_modes() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));

    let r = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    assert!(r["sessionId"].as_str().unwrap().len() > 0);
    assert_eq!(r["modes"]["currentModeId"], "full_access");
    let mode_ids: Vec<&str> = r["modes"]["availableModes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(mode_ids.contains(&"full_access"));
    assert!(mode_ids.contains(&"confirm"));
    assert!(mode_ids.contains(&"watch"));

    let opts = r["configOptions"].as_array().unwrap();
    let ids: Vec<&str> = opts.iter().map(|o| o["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"policy_mode"));
    assert!(ids.contains(&"model"));
    assert!(ids.contains(&"reasoning_mode"));
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests: session/list
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_list_sessions_returns_created_session() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    let r = c.send_request("session/list", json!({}));
    let sessions = r["sessions"].as_array().unwrap();
    let ids: Vec<&str> = sessions
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&sid));
}

#[test]
fn test_list_sessions_multiple() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));

    let s1 = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let s2 = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid1 = s1["sessionId"].as_str().unwrap();
    let sid2 = s2["sessionId"].as_str().unwrap();
    assert_ne!(sid1, sid2);

    let r = c.send_request("session/list", json!({}));
    let ids: Vec<&str> = r["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&sid1));
    assert!(ids.contains(&sid2));
}

#[test]
fn test_list_sessions_with_cursor_returns_empty() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));

    // With cursor, should return empty (no next page).
    let r = c.send_request("session/list", json!({"cursor": "some-cursor"}));
    let sessions = r["sessions"].as_array().unwrap();
    assert!(sessions.is_empty());
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests: session/prompt
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_prompt_returns_end_turn() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    let r = c.send_request(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Hi"}]}),
    );
    assert_eq!(r["stopReason"], "end_turn");

    let notifs = c.drain_notifications();
    let has_msg = notifs.iter().any(|n| {
        n.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("agent_message_chunk")
    });
    assert!(has_msg, "Expected agent_message_chunk, got: {notifs:?}");
}

#[test]
fn test_prompt_emits_usage_update() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();
    c.clear_notifications();

    c.send_request(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Hi"}]}),
    );

    let notifs = c.drain_notifications();
    let usage = notifs
        .iter()
        .filter_map(usage_update)
        .last()
        .expect("Expected usage_update notification");
    assert_eq!(usage["used"], 15);
    assert!(
        usage["size"].as_u64().unwrap() >= 15,
        "usage size should expose the context window, got: {usage}"
    );
}

#[test]
fn test_load_session_emits_persisted_usage_update() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let sid = {
        let mut c = AcpClient::spawn(&folder);
        c.send_request("initialize", json!({"protocolVersion": 1}));
        let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
        let sid = s["sessionId"].as_str().unwrap().to_string();
        c.send_request(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Hi"}]}),
        );
        sid
    };

    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    c.send_request(
        "session/load",
        json!({"cwd": ".", "sessionId": sid, "mcpServers": []}),
    );

    let notifs = c.drain_notifications();
    let usage = notifs
        .iter()
        .filter_map(usage_update)
        .last()
        .expect("Expected persisted usage_update notification on load");
    assert_eq!(usage["used"], 15);
    assert!(
        usage["size"].as_u64().unwrap() >= 15,
        "usage size should expose the context window, got: {usage}"
    );
}

#[test]
fn test_prompt_user_message_replayed_on_load() {
    // user_message_chunk is recorded in transcript and replayed on session/load.
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap().to_string();

    c.send_request(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Hello world"}]}),
    );
    c.drain_notifications();
    c.clear_notifications();

    // Load session — should replay user_message_chunk from transcript.
    c.send_request(
        "session/load",
        json!({"cwd": ".", "sessionId": sid, "mcpServers": []}),
    );
    let notifs = c.drain_notifications();
    let has_user_msg = notifs.iter().any(|n| {
        n.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("user_message_chunk")
    });
    assert!(
        has_user_msg,
        "Expected user_message_chunk in replayed transcript, got: {notifs:?}"
    );
}

#[test]
fn test_prompt_sets_session_title() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    c.send_request(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "My first prompt"}]}),
    );

    let notifs = c.drain_notifications();
    let has_session_info = notifs.iter().any(|n| {
        n.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
    });
    assert!(has_session_info, "Expected session_info_update with title");
}

#[test]
fn test_prompt_multiple_turns_same_session() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    let r1 = c.send_request(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "First"}]}),
    );
    assert_eq!(r1["stopReason"], "end_turn");

    c.clear_notifications();
    let r2 = c.send_request(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Second"}]}),
    );
    assert_eq!(r2["stopReason"], "end_turn");

    // Verify LLM was called twice.
    assert!(mock.request_count.load(Ordering::SeqCst) >= 2);
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests: session/cancel
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_cancel_idle_session_is_noop() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    // Cancel on idle session should be a no-op, agent stays alive.
    c.send_notification("session/cancel", json!({"sessionId": sid}));
    thread::sleep(Duration::from_millis(200));

    // Agent should still respond to requests.
    let r = c.send_request("session/list", json!({}));
    assert!(r["sessions"].is_array());
}

#[test]
fn test_cancel_during_streaming_returns_cancelled() {
    // Use a slow LLM that takes 10s to respond — cancel should interrupt it.
    let mock = SlowStreamingLlm::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap().to_string();

    // Fire prompt (will block waiting for slow LLM).
    let prompt_id = c.send_request_no_wait(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Slow request"}]}),
    );

    // Give agent time to start streaming.
    thread::sleep(Duration::from_millis(800));

    // Send cancel.
    c.send_notification("session/cancel", json!({"sessionId": sid}));

    // The prompt response should come back within a few seconds (not 10s).
    let resp = c.wait_response(prompt_id, Duration::from_secs(8));
    assert!(
        resp.is_some(),
        "Expected prompt to return after cancel, but timed out"
    );
    let resp = resp.unwrap();
    let result = resp.get("result").cloned().unwrap_or(Value::Null);
    // Should be cancelled stop reason.
    assert_eq!(
        result["stopReason"], "cancelled",
        "Expected cancelled stop reason, got: {result}"
    );
}

#[test]
fn test_cancel_nonexistent_session_is_safe() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));

    // Cancel a session that doesn't exist — should not crash.
    c.send_notification(
        "session/cancel",
        json!({"sessionId": "nonexistent-session-id"}),
    );
    thread::sleep(Duration::from_millis(200));

    // Agent should still be alive.
    let r = c.send_request("session/list", json!({}));
    assert!(r["sessions"].is_array());
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests: Prompt Preemption (new prompt while running cancels old)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_prompt_preemption_cancels_previous() {
    // Use slow LLM so first prompt is still running when second arrives.
    let mock = SlowStreamingLlm::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap().to_string();

    // Fire first prompt (will be slow).
    let first_id = c.send_request_no_wait(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "First slow"}]}),
    );

    // Wait for it to start streaming.
    thread::sleep(Duration::from_millis(800));

    // Fire second prompt — should preempt the first.
    // The agent's run_prompt checks is_active() and calls cancel() before proceeding.
    let second_id = c.send_request_no_wait(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Second preempts"}]}),
    );

    // First prompt should return cancelled.
    let first_resp = c.wait_response(first_id, Duration::from_secs(8));
    assert!(
        first_resp.is_some(),
        "First prompt should have returned after preemption"
    );
    let first_result = first_resp
        .unwrap()
        .get("result")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        first_result["stopReason"], "cancelled",
        "First prompt should be cancelled, got: {first_result}"
    );

    // Second prompt will also be slow (same LLM), cancel it too.
    thread::sleep(Duration::from_millis(500));
    c.send_notification("session/cancel", json!({"sessionId": sid}));
    let second_resp = c.wait_response(second_id, Duration::from_secs(8));
    assert!(second_resp.is_some(), "Second prompt should have returned");
    let second_result = second_resp
        .unwrap()
        .get("result")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(second_result["stopReason"], "cancelled");
}

#[test]
fn test_policy_mode_change_applies_to_next_tool_call_in_running_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let patch =
        "*** Begin Patch\n*** Add File: live_policy.txt\n+policy switched live\n*** End Patch\n"
            .to_string();
    let mock = ToolCallingLlm::start_with_first_response_delay(Duration::from_millis(800), patch);
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request(
        "session/new",
        json!({"cwd": workspace.to_string_lossy(), "mcpServers": []}),
    );
    let sid = s["sessionId"].as_str().unwrap().to_string();

    let confirm_result = c.send_request(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "policy_mode", "value": "confirm"}),
    );
    assert_eq!(
        config_current_value(&confirm_result, "policy_mode"),
        "confirm"
    );

    let prompt_id = c.send_request_no_wait(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Create the file after policy changes"}]}),
    );
    thread::sleep(Duration::from_millis(250));

    let allow_result = c.send_request(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "policy_mode", "value": "full_access"}),
    );
    assert_eq!(
        config_current_value(&allow_result, "policy_mode"),
        "full_access"
    );

    let prompt_resp = c
        .wait_response(prompt_id, Duration::from_secs(8))
        .expect("prompt should finish without a permission response after live policy switch");
    let prompt_result = prompt_resp.get("result").cloned().unwrap_or(Value::Null);
    assert_eq!(
        prompt_result["stopReason"], "end_turn",
        "got: {prompt_result}"
    );
    assert_eq!(
        read_utf8_bom_text(workspace.join("live_policy.txt")),
        "policy switched live\n"
    );
    assert!(
        mock.request_count() >= 2,
        "tool result should be sent back to the model for a follow-up turn"
    );
}

#[test]
fn test_live_config_changes_return_immediately_and_apply_to_preempting_prompt() {
    let mock = SlowStreamingLlm::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap().to_string();

    let first_id = c.send_request_no_wait(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "First slow"}]}),
    );
    thread::sleep(Duration::from_millis(800));

    let policy_id = c.send_request_no_wait(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "policy_mode", "value": "watch"}),
    );
    let policy_resp = c
        .wait_response(policy_id, Duration::from_secs(2))
        .expect("policy change should respond while prompt is running");
    let policy_result = policy_resp.get("result").cloned().unwrap_or(Value::Null);
    assert_eq!(config_current_value(&policy_result, "policy_mode"), "watch");

    let model_id = c.send_request_no_wait(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "model", "value": "mock-flash"}),
    );
    let model_resp = c
        .wait_response(model_id, Duration::from_secs(2))
        .expect("model change should respond while prompt is running");
    let model_result = model_resp.get("result").cloned().unwrap_or(Value::Null);
    assert_eq!(config_current_value(&model_result, "model"), "mock-flash");

    let reasoning_id = c.send_request_no_wait(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "reasoning_mode", "value": "max"}),
    );
    let reasoning_resp = c
        .wait_response(reasoning_id, Duration::from_secs(2))
        .expect("reasoning change should respond while prompt is running");
    let reasoning_result = reasoning_resp.get("result").cloned().unwrap_or(Value::Null);
    assert_eq!(
        config_current_value(&reasoning_result, "reasoning_mode"),
        "max"
    );

    let second_id = c.send_request_no_wait(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Second uses pending config"}]}),
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while mock.requests().len() < 2 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    let requests = mock.requests();
    assert!(
        requests.len() >= 2,
        "second prompt should start after preempting the first"
    );
    assert_eq!(requests[1]["model"], "deepseek-v4-flash");
    assert_eq!(requests[1]["reasoning_effort"], "max");

    let first_resp = c.wait_response(first_id, Duration::from_secs(8));
    assert!(first_resp.is_some(), "first prompt should be cancelled");

    c.send_notification("session/cancel", json!({"sessionId": sid}));
    let second_resp = c.wait_response(second_id, Duration::from_secs(8));
    assert!(second_resp.is_some(), "second prompt should be cancellable");
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests: Invalid Request Rejection
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_prompt_empty_prompt_rejected() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    // Empty prompt array should be rejected.
    let resp = c.send_request_raw("session/prompt", json!({"sessionId": sid, "prompt": []}));
    assert!(
        resp.get("error").is_some(),
        "Expected error for empty prompt, got: {resp}"
    );
}

#[test]
fn test_prompt_whitespace_only_text_rejected() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    // Whitespace-only text should be rejected.
    let resp = c.send_request_raw(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "   "}]}),
    );
    assert!(
        resp.get("error").is_some(),
        "Expected error for whitespace-only text, got: {resp}"
    );
}

#[test]
fn test_prompt_unknown_session_rejected() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));

    // Prompt with non-existent session id.
    let resp = c.send_request_raw(
        "session/prompt",
        json!({"sessionId": "does-not-exist", "prompt": [{"type": "text", "text": "Hi"}]}),
    );
    assert!(
        resp.get("error").is_some(),
        "Expected error for unknown session, got: {resp}"
    );
}

#[test]
fn test_prompt_image_without_data_or_uri_rejected() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    // Image block with empty data and no uri.
    let resp = c.send_request_raw(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "image", "data": "", "mimeType": "image/png"}]}),
    );
    assert!(
        resp.get("error").is_some(),
        "Expected error for image without data/uri, got: {resp}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests: session/set_mode and session/set_config_option
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_set_mode_emits_notifications() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    c.send_request(
        "session/set_mode",
        json!({"sessionId": sid, "modeId": "confirm"}),
    );
    let notifs = c.drain_notifications();
    let types: Vec<&str> = notifs
        .iter()
        .filter_map(|n| {
            n.pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
        })
        .collect();
    assert!(types.contains(&"current_mode_update"), "got: {types:?}");
    assert!(types.contains(&"config_option_update"), "got: {types:?}");
}

#[test]
fn test_set_mode_invalid_mode_rejected() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    let resp = c.send_request_raw(
        "session/set_mode",
        json!({"sessionId": sid, "modeId": "invalid_mode_xyz"}),
    );
    assert!(
        resp.get("error").is_some(),
        "Expected error for invalid mode, got: {resp}"
    );
}

#[test]
fn test_set_config_option_policy() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    let r = c.send_request(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "policy_mode", "value": "watch"}),
    );
    let policy = r["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "policy_mode")
        .unwrap();
    assert_eq!(policy["currentValue"], "watch");
}

#[test]
fn test_set_config_option_unsupported_rejected() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap();

    let resp = c.send_request_raw(
        "session/set_config_option",
        json!({"sessionId": sid, "configId": "nonexistent_option", "value": "foo"}),
    );
    assert!(
        resp.get("error").is_some(),
        "Expected error for unsupported config option, got: {resp}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests: session/load
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_load_session_not_found() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));

    let resp = c.send_request_raw(
        "session/load",
        json!({"cwd": ".", "sessionId": "nonexistent-id-12345", "mcpServers": []}),
    );
    assert!(
        resp.get("error").is_some(),
        "Expected error for loading nonexistent session, got: {resp}"
    );
}

#[test]
fn test_load_session_after_prompt() {
    let mock = MockLlmServer::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_mock_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);
    c.send_request("initialize", json!({"protocolVersion": 1}));
    let s = c.send_request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap().to_string();

    // Run a prompt to generate transcript.
    c.send_request(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Hello"}]}),
    );
    c.drain_notifications();
    c.clear_notifications();

    // Load the same session — should replay transcript events.
    let r = c.send_request(
        "session/load",
        json!({"cwd": ".", "sessionId": sid, "mcpServers": []}),
    );
    assert!(r["modes"].is_object());
    assert!(r["configOptions"].is_array());

    // Should have replayed notifications.
    let notifs = c.drain_notifications();
    assert!(
        !notifs.is_empty(),
        "Expected replayed transcript notifications on load"
    );
}
