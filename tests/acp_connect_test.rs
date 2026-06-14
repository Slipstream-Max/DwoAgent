//! ACP stdio bridge integration test.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TEST_TOKEN: &str =
    "dwo_stdio_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn acp_connect_bridges_stdio_to_running_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = create_acp_agent_folder(tmp.path());
    let mut server = ServerProcess::spawn(&folder);
    wait_for_daemon_manifest(&folder, &mut server);

    let mut bridge = AcpBridgeProcess::spawn(&folder);

    let initialize = bridge.send_request(
        1,
        "initialize",
        json!({
            "protocolVersion": 1
        }),
    );
    assert_eq!(initialize["protocolVersion"], 1);
    assert_eq!(initialize["agentInfo"]["name"], "ACP Connect Test Agent");

    let session = bridge.send_request(
        2,
        "session/new",
        json!({
            "cwd": ".",
            "mcpServers": []
        }),
    );
    assert!(session["sessionId"].as_str().is_some());
    assert_eq!(session["modes"]["currentModeId"], "full_access");

    drop(bridge);
    assert!(
        server.exit_message().is_none(),
        "serve should keep running after acp connect exits"
    );
}

fn wait_for_daemon_manifest(folder: &Path, server: &mut ServerProcess) {
    let path = folder
        .join("runtime")
        .join("channel_secret")
        .join("stdio")
        .join("daemon.yaml");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(message) = server.exit_message() {
            panic!("serve exited before daemon manifest was written: {message}");
        }
        if path.is_file() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {}", path.display());
}

fn create_acp_agent_folder(tmp_dir: &Path) -> PathBuf {
    let agent_dir = tmp_dir.join("agent");
    let prompt_dir = agent_dir.join("resources").join("prompt");
    std::fs::create_dir_all(&prompt_dir).unwrap();

    std::fs::write(
        agent_dir.join("agent.yaml"),
        "\
agent_id: acp-connect-test-agent
name: ACP Connect Test Agent
description: Test agent for ACP connect bridge
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
  stdio:
    enabled: true
    auth: true
",
    )
    .unwrap();
    let secret_dir = agent_dir
        .join("runtime")
        .join("channel_secret")
        .join("stdio");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::write(
        secret_dir.join("auth.yaml"),
        format!("token: \"{TEST_TOKEN}\"\n"),
    )
    .unwrap();

    std::fs::write(
        prompt_dir.join("system.md"),
        "You are an ACP connect test agent.\n",
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

struct AcpBridgeProcess {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
}

impl AcpBridgeProcess {
    fn spawn(agent_folder: &Path) -> Self {
        let binary = agent_binary_path();
        let mut child = Command::new(&binary)
            .args([
                "acp",
                "connect",
                "--agent-folder",
                agent_folder.to_str().unwrap(),
            ])
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {}: {err}", binary.display()));
        let stdin = child.stdin.take().expect("bridge stdin");
        let stdout = child.stdout.take().expect("bridge stdout");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        Self {
            child,
            stdin,
            lines: rx,
        }
    }

    fn send_request(&mut self, id: u64, method: &str, params: Value) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(message) = self.exit_message() {
                panic!("acp bridge exited while waiting for {method}: {message}");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for ACP response to {method}");
            }
            let line = match self
                .lines
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(err) => panic!("bridge stdout closed before {method} response: {err}"),
            };
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                panic!("ACP error for {method}: {error}");
            }
            return value.get("result").cloned().unwrap_or(Value::Null);
        }
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

impl Drop for AcpBridgeProcess {
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
