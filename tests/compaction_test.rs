//! Compaction integration test.
//!
//! Verifies that context compaction triggers when token usage exceeds threshold.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

// ── Mock LLM ───────────────────────────────────────────────────────────────

struct CompactionMockLlm {
    _handle: thread::JoinHandle<()>,
    port: u16,
    compaction_count: Arc<AtomicUsize>,
}

impl CompactionMockLlm {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let compaction_count = Arc::new(AtomicUsize::new(0));
        let cc = compaction_count.clone();

        let handle = thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let cc = cc.clone();
                thread::spawn(move || handle_http_request(stream, cc));
            }
        });
        thread::sleep(Duration::from_millis(50));
        Self {
            _handle: handle,
            port,
            compaction_count,
        }
    }
}

fn handle_http_request(mut stream: std::net::TcpStream, compaction_count: Arc<AtomicUsize>) {
    // Read the full HTTP request.
    let mut buf_reader = BufReader::new(&stream);
    let mut request_line = String::new();
    buf_reader.read_line(&mut request_line).ok();

    let mut content_length: usize = 0;
    loop {
        let mut header_line = String::new();
        buf_reader.read_line(&mut header_line).ok();
        let trimmed = header_line.trim().to_lowercase();
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("content-length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }

    let mut body_bytes = vec![0u8; content_length];
    buf_reader.read_exact(&mut body_bytes).ok();
    let request: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    let is_stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_compaction = request
        .get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter().any(|m| {
                m.get("role").and_then(Value::as_str) == Some("system")
                    && m.get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .contains("CONTEXT CHECKPOINT COMPACTION")
            })
        })
        .unwrap_or(false);

    if is_compaction {
        compaction_count.fetch_add(1, Ordering::SeqCst);
    }

    let response_text = if is_compaction {
        "Summary: The user said hello."
    } else {
        "Hello from mock LLM with a long response to inflate tokens."
    };

    // Report high token usage: with compact_threshold=0.0001 and
    // context_window=1024000, trigger = 102 tokens. Report 120.
    let usage = json!({"prompt_tokens": 80, "completion_tokens": 40, "total_tokens": 120});

    if is_stream {
        let c1 = json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]});
        let c2 = json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":response_text}}]});
        let c3 = json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{}}],"usage":usage});
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::to_string(&c1).unwrap(),
            serde_json::to_string(&c2).unwrap(),
            serde_json::to_string(&c3).unwrap(),
        );
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        stream.write_all(resp.as_bytes()).ok();
        stream.flush().ok();
    } else {
        let json_body = json!({
            "id": "c",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": response_text}, "finish_reason": "stop"}],
            "usage": usage
        });
        let payload = serde_json::to_string(&json_body).unwrap();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream.write_all(resp.as_bytes()).ok();
        stream.flush().ok();
    }
    // Let the client read the response before we close.
    thread::sleep(Duration::from_millis(10));
}

// ── Agent folder ───────────────────────────────────────────────────────────

fn create_agent_folder(tmp_dir: &Path, llm_port: u16) -> PathBuf {
    let agent_dir = tmp_dir.join("agent");
    let resources_dir = agent_dir.join("resources");
    let prompt_dir = resources_dir.join("prompt");
    let skills_dir = resources_dir.join("skills");
    std::fs::create_dir_all(&prompt_dir).unwrap();
    std::fs::create_dir_all(&skills_dir).unwrap();

    std::fs::write(
        agent_dir.join("agent.yaml"),
        format!(
            "\
agent_id: test-agent
name: Compaction Test Agent
description: Test
max_running_turn: 5
policy_mode: full_access
session_store_dir: .sessions
model:
  default_model_id: mock-model
  models:
    - model_name: mock-model
      provider: deepseek
      model_id: deepseek-v4-pro
      api_key: test-key
      api_base: http://127.0.0.1:{llm_port}/v1
      default_reasoning_mode: auto
      compact_threshold: 0.0001
"
        ),
    )
    .unwrap();

    std::fs::write(prompt_dir.join("system.md"), "You are a test agent.\n").unwrap();
    agent_dir
}

// ── ACP Client ─────────────────────────────────────────────────────────────

struct AcpClient {
    stdin: std::process::ChildStdin,
    responses: std::sync::mpsc::Receiver<Value>,
    request_id: AtomicU64,
    _child: Child,
}

impl AcpClient {
    fn spawn(agent_folder: &Path) -> Self {
        let binary = find_binary();
        let mut child = Command::new(&binary)
            .args([
                "acp",
                "embedded",
                "--agent-folder",
                agent_folder.to_str().unwrap(),
            ])
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn: {e}"));

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Value>();

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
                }
                // Notifications are ignored for this test.
            }
        });

        thread::sleep(Duration::from_millis(500));
        if let Some(status) = child.try_wait().ok().flatten() {
            let mut err = String::new();
            if let Some(mut se) = child.stderr.take() {
                se.read_to_string(&mut err).ok();
            }
            panic!("Agent exited: {status}\n{err}");
        }

        Self {
            stdin,
            responses: rx,
            request_id: AtomicU64::new(0),
            _child: child,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst) + 1;
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(self.stdin, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            match self.responses.recv_timeout(Duration::from_millis(100)) {
                Ok(r) if r.get("id").and_then(Value::as_u64) == Some(id) => {
                    if let Some(e) = r.get("error") {
                        panic!("ACP error: {e}");
                    }
                    return r.get("result").cloned().unwrap_or(Value::Null);
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() > deadline {
                        panic!("Timeout for {method}");
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("Disconnected waiting for {method}");
                }
            }
        }
    }
}

fn find_binary() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for sub in ["debug", "release"] {
        let name = if cfg!(windows) {
            "dwo-agent.exe"
        } else {
            "dwo-agent"
        };
        let p = dir.join("target").join(sub).join(name);
        if p.exists() {
            return p;
        }
    }
    panic!("dwo-agent not found");
}

// ── Test ───────────────────────────────────────────────────────────────────

#[test]
fn test_compaction_triggers_after_token_threshold() {
    let mock = CompactionMockLlm::start();
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_agent_folder(tmp.path(), mock.port);
    let mut c = AcpClient::spawn(&folder);

    c.request("initialize", json!({"protocolVersion": 1}));
    let s = c.request("session/new", json!({"cwd": ".", "mcpServers": []}));
    let sid = s["sessionId"].as_str().unwrap().to_string();

    assert_eq!(mock.compaction_count.load(Ordering::SeqCst), 0);

    // First prompt: mock reports 120 total_tokens > trigger(102).
    // Compaction should fire during maybe_compact after the turn.
    c.request(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [{"type": "text", "text": "Hello"}]
        }),
    );

    let count = mock.compaction_count.load(Ordering::SeqCst);
    assert!(
        count >= 1,
        "Expected compaction after first prompt, got count={count}"
    );

    // Second prompt should also trigger.
    c.request(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [{"type": "text", "text": "Second"}]
        }),
    );

    let count2 = mock.compaction_count.load(Ordering::SeqCst);
    assert!(count2 >= 2, "Expected 2+ compactions, got count={count2}");
}
