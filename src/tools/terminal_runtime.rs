//! Terminal executor and session implementation.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;

use super::session::{Cap, ToolArgs, ToolSession};

const DEFAULT_TAIL_LINE_NUM: usize = 200;

pub struct TerminalHandle {
    state: Arc<Mutex<HandleState>>,
    done: Arc<Notify>,
    pid: Option<u32>,
}

struct HandleState {
    output_lines: Vec<String>,
    exit_code: Option<i32>,
    killed: bool,
    finished: bool,
}

impl HandleState {
    fn new() -> Self {
        Self {
            output_lines: Vec::new(),
            exit_code: None,
            killed: false,
            finished: false,
        }
    }

    fn status_label(&self) -> &'static str {
        if self.killed {
            "killed"
        } else if self.exit_code == Some(0) {
            "completed_success"
        } else {
            "completed_error"
        }
    }
}

#[derive(Clone)]
pub struct TerminalExecutor {
    cwd: Option<PathBuf>,
}

impl TerminalExecutor {
    pub fn new(cwd: Option<PathBuf>) -> Self {
        let resolved = cwd.map(|p| std::fs::canonicalize(&p).unwrap_or(p));
        Self { cwd: resolved }
    }

    /// Spawn the command and set up background readers.
    pub async fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<TerminalHandle> {
        let command_text = command.trim();
        if command_text.is_empty() {
            bail!("Missing argument: command");
        }

        let mut cmd = self.build_command(command_text);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in self.merged_env(env) {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let state = Arc::new(Mutex::new(HandleState::new()));
        let done = Arc::new(Notify::new());

        let mut readers = Vec::new();
        if let Some(stdout) = stdout {
            readers.push(Self::spawn_reader(stdout, state.clone()));
        }
        if let Some(stderr) = stderr {
            readers.push(Self::spawn_reader(stderr, state.clone()));
        }

        let child_arc = Arc::new(AsyncMutex::new(Some(child)));
        Self::spawn_watcher(child_arc.clone(), readers, state.clone(), done.clone());

        Ok(TerminalHandle { state, done, pid })
    }

    async fn wait_for_exit(&self, handle: &TerminalHandle, timeout_secs: f64) -> bool {
        if lock_state(&handle.state).finished {
            return true;
        }
        let timeout = Duration::from_secs_f64(positive_float(timeout_secs, 30.0));
        tokio::select! {
            _ = handle.done.notified() => true,
            _ = tokio::time::sleep(timeout) => lock_state(&handle.state).finished,
        }
    }

    /// Kill the process tree if still running. Mirror of `_executor.kill`.
    pub async fn kill(&self, handle: &TerminalHandle) -> Result<()> {
        if lock_state(&handle.state).finished {
            return Ok(());
        }
        {
            let mut state = lock_state(&handle.state);
            state.killed = true;
        }
        if let Some(pid) = handle.pid {
            kill_process(pid).await?;
        }
        Ok(())
    }

    // ── Internals ──────────────────────────────────────────────────────────

    fn build_command(&self, command: &str) -> Command {
        if cfg!(windows) {
            let mut cmd = Command::new(windows_shell_program());
            let ps_script = format!("$ProgressPreference='SilentlyContinue';{command}");
            cmd.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &ps_script,
            ]);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    }

    fn merged_env(&self, env: Option<&HashMap<String, String>>) -> HashMap<String, String> {
        let mut merged: HashMap<String, String> = std::env::vars().collect();
        if let Some(extra) = env {
            for (k, v) in extra {
                merged.insert(k.clone(), v.clone());
            }
        }
        merged
    }

    fn spawn_reader(
        stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        state: Arc<Mutex<HandleState>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let mut s = lock_state(&state);
                        s.output_lines
                            .push(String::from_utf8_lossy(&buf).into_owned());
                    }
                    Err(_) => break,
                }
            }
        })
    }

    fn spawn_watcher(
        child: Arc<AsyncMutex<Option<Child>>>,
        readers: Vec<JoinHandle<()>>,
        state: Arc<Mutex<HandleState>>,
        done: Arc<Notify>,
    ) {
        tokio::spawn(async move {
            let status = {
                let mut guard = child.lock().await;
                match guard.as_mut() {
                    Some(c) => c.wait().await.ok(),
                    None => None,
                }
            };
            for reader in readers {
                let _ = reader.await;
            }
            let exit_code = status.and_then(|s| s.code());
            {
                let mut s = lock_state(&state);
                s.exit_code = exit_code;
                s.finished = true;
            }
            done.notify_waiters();
        });
    }
}

// ── Session ────────────────────────────────────────────────────────────────

/// One terminal command run.
pub struct TerminalSession {
    session_id: String,
    executor: TerminalExecutor,
    command: String,
    env: Option<HashMap<String, String>>,
    timeout: f64,
    handle: Option<TerminalHandle>,
}

impl TerminalSession {
    pub fn new(
        session_id: impl Into<String>,
        executor: TerminalExecutor,
        command: impl Into<String>,
        env: Option<HashMap<String, String>>,
        timeout: f64,
    ) -> Self {
        let command_text = command.into().trim().to_string();
        Self {
            session_id: session_id.into(),
            executor,
            command: command_text,
            env,
            timeout: positive_float(timeout, 30.0),
            handle: None,
        }
    }

    fn render_snapshot(
        &self,
        handle: &TerminalHandle,
        tail_line_num: usize,
        message: Option<&str>,
    ) -> Value {
        let state = lock_state(&handle.state);
        let (status, done, exit_code) = if state.finished {
            (state.status_label(), true, state.exit_code)
        } else {
            ("running", false, None)
        };
        let mut payload = Map::new();
        payload.insert("id".to_string(), Value::String(self.session_id.clone()));
        payload.insert(
            "runtime".to_string(),
            json!({
                "kind": "terminal",
                "id": self.session_id,
                "status": status,
            }),
        );
        payload.insert("status".to_string(), Value::String(status.to_string()));
        payload.insert("done".to_string(), Value::Bool(done));
        if done {
            payload.insert(
                "exit_code".to_string(),
                exit_code
                    .map(|v| Value::Number(v.into()))
                    .unwrap_or(Value::Null),
            );
        }
        let total_lines = state.output_lines.len();
        let start = total_lines.saturating_sub(tail_line_num.max(1));
        payload.insert(
            "output".to_string(),
            Value::String(state.output_lines[start..].concat()),
        );
        payload.insert(
            "returned_lines".to_string(),
            Value::Number((total_lines - start).into()),
        );
        payload.insert("total_lines".to_string(), Value::Number(total_lines.into()));
        if let Some(msg) = message {
            payload.insert("message".to_string(), Value::String(msg.to_string()));
        }
        Value::Object(payload)
    }
}

#[async_trait]
impl ToolSession for TerminalSession {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn capabilities(&self) -> HashSet<Cap> {
        let mut caps = HashSet::new();
        caps.insert(Cap::Checkout);
        caps
    }

    async fn start(&mut self, _args: &ToolArgs) -> Result<Value> {
        if self.handle.is_some() {
            return self.checkout(_args).await;
        }
        match self.executor.exec(&self.command, self.env.as_ref()).await {
            Ok(handle) => {
                self.handle = Some(handle);
            }
            Err(exc) => {
                return Ok(json!({
                    "id": self.session_id.clone(),
                    "runtime": {
                        "kind": "terminal",
                        "id": self.session_id.clone(),
                        "status": "completed_error",
                    },
                    "status": "completed_error",
                    "done": true,
                    "error": exc.to_string(),
                }));
            }
        }
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| anyhow!("terminal session has not started"))?;
        let finished = self.executor.wait_for_exit(handle, self.timeout).await;
        let message = (!finished).then_some("command still running after timeout");
        Ok(self.render_snapshot(handle, DEFAULT_TAIL_LINE_NUM, message))
    }

    async fn checkout(&mut self, args: &ToolArgs) -> Result<Value> {
        let tail_line_num = args
            .get("tail_line_num")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TAIL_LINE_NUM);
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| anyhow!("terminal session has not started"))?;
        Ok(self.render_snapshot(handle, tail_line_num, None))
    }

    async fn cancel(&mut self) -> Result<()> {
        let Some(handle) = &self.handle else {
            return Ok(());
        };
        let _ = self.executor.kill(handle).await;
        let _ = self.executor.wait_for_exit(handle, 2.0).await;
        Ok(())
    }

    fn is_done(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| lock_state(&h.state).finished)
            .unwrap_or(false)
    }

    fn list_item(&self) -> Value {
        let status = self
            .handle
            .as_ref()
            .map(|handle| {
                let state = lock_state(&handle.state);
                if state.finished {
                    state.status_label()
                } else {
                    "running"
                }
            })
            .unwrap_or("created");
        json!({
            "id": self.session_id,
            "kind": "terminal",
            "status": status,
            "done": self.is_done(),
        })
    }
}

// ── Free helpers ───────────────────────────────────────────────────────────

pub fn terminal_not_found(id: &str) -> Value {
    json!({
        "id": id,
        "runtime": {
            "kind": "terminal",
            "id": id,
            "status": "not_found",
        },
        "status": "not_found",
        "done": true,
    })
}

pub async fn terminal_wait(time_secs: f64) -> Result<Value> {
    if !time_secs.is_finite() || time_secs <= 0.0 {
        bail!("time must be a positive number.");
    }
    tokio::time::sleep(Duration::from_secs_f64(time_secs)).await;
    Ok(json!({
        "status": "ok",
        "done": true,
        "waited_seconds": time_secs,
    }))
}

fn positive_float(value: f64, default: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        default.max(0.1)
    } else {
        value.max(0.1)
    }
}

fn lock_state(state: &Mutex<HandleState>) -> MutexGuard<'_, HandleState> {
    state.lock().expect("terminal state lock poisoned")
}

fn windows_shell_program() -> &'static str {
    if cfg!(windows) && command_exists_on_path("pwsh.exe") {
        "pwsh.exe"
    } else {
        "powershell.exe"
    }
}

fn command_exists_on_path(exe_name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(exe_name).is_file()))
        .unwrap_or(false)
}

// ── Kill process tree ──────────────────────────────────────────────────────

async fn kill_process(pid: u32) -> Result<()> {
    let pid_arg = pid.to_string();
    let (program, args, failure_message): (&str, Vec<&str>, String) = if cfg!(windows) {
        (
            "taskkill",
            vec!["/PID", pid_arg.as_str(), "/T", "/F"],
            format!("taskkill failed for process tree rooted at pid {pid}"),
        )
    } else {
        (
            "kill",
            vec!["-KILL", pid_arg.as_str()],
            format!("kill failed for pid {pid}"),
        )
    };
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        bail!(failure_message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::AsyncWriteExt;

    fn quick_command() -> &'static str {
        if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        }
    }

    fn non_ascii_command() -> &'static str {
        if cfg!(windows) {
            "Write-Output '赤铎 — ok'"
        } else {
            "printf '赤铎 — ok\\n'"
        }
    }

    #[tokio::test]
    async fn wait_after_process_already_finished_returns_immediately() {
        let executor = TerminalExecutor::new(None);
        let handle = executor.exec(quick_command(), None).await.unwrap();

        assert!(executor.wait_for_exit(&handle, 5.0).await);

        let started = Instant::now();
        assert!(executor.wait_for_exit(&handle, 5.0).await);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "second wait should observe the completed state without waiting for timeout"
        );
    }

    #[tokio::test]
    async fn terminal_session_start_returns_completed_success() {
        let executor = TerminalExecutor::new(None);
        let mut session =
            TerminalSession::new("terminal-test", executor, quick_command(), None, 5.0);

        let output = session.start(&Map::new()).await.unwrap();
        assert_eq!(output["status"], "completed_success");
        assert_eq!(output["done"], true);
        assert_eq!(output["output"].as_str().unwrap().trim_end(), "done");
    }

    #[tokio::test]
    async fn terminal_session_keeps_non_ascii_output() {
        let executor = TerminalExecutor::new(None);
        let mut session = TerminalSession::new(
            "terminal-non-ascii",
            executor,
            non_ascii_command(),
            None,
            5.0,
        );

        let output = session.start(&Map::new()).await.unwrap();

        assert_eq!(output["status"], "completed_success");
        assert_eq!(output["done"], true);
        assert_eq!(output["total_lines"], 1);
        assert_eq!(output["output"].as_str().unwrap().trim_end(), "赤铎 — ok");
    }

    #[tokio::test]
    async fn terminal_reader_decodes_invalid_utf8_lossy() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let state = Arc::new(Mutex::new(HandleState::new()));
        let reader_task = TerminalExecutor::spawn_reader(reader, state.clone());

        writer.write_all(&[0xC3, 0x28, b'\n']).await.unwrap();
        drop(writer);
        reader_task.await.unwrap();

        let lines = lock_state(&state).output_lines.clone();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains('\u{FFFD}'));
        assert!(lines[0].ends_with('\n'));
    }

    #[tokio::test]
    async fn terminal_session_waits_for_reader_output_before_finalize() {
        let tmp = tempfile::tempdir().unwrap();
        let file_text = (1..=50).map(|i| format!("line{i}\n")).collect::<String>();
        std::fs::write(tmp.path().join("sample.txt"), file_text).unwrap();
        let command = if cfg!(windows) {
            "Get-Content sample.txt -First 20"
        } else {
            "head -20 sample.txt"
        };
        let executor = TerminalExecutor::new(Some(tmp.path().to_path_buf()));
        let mut session = TerminalSession::new("terminal-drain", executor, command, None, 5.0);

        let output = session.start(&Map::new()).await.unwrap();

        assert_eq!(output["status"], "completed_success");
        assert_eq!(output["done"], true);
        assert_eq!(output["total_lines"], 20);
        let text = output["output"].as_str().unwrap();
        assert!(text.contains("line1"));
        assert!(text.contains("line20"));
    }

    #[tokio::test]
    async fn terminal_session_checkout_respects_tail_line_num() {
        let tmp = tempfile::tempdir().unwrap();
        let file_text = (1..=10).map(|i| format!("line{i}\n")).collect::<String>();
        std::fs::write(tmp.path().join("sample.txt"), file_text).unwrap();
        let command = if cfg!(windows) {
            "Get-Content sample.txt"
        } else {
            "cat sample.txt"
        };
        let executor = TerminalExecutor::new(Some(tmp.path().to_path_buf()));
        let mut session = TerminalSession::new("terminal-tail", executor, command, None, 5.0);

        let first = session.start(&Map::new()).await.unwrap();

        assert_eq!(first["status"], "completed_success");
        assert_eq!(first["total_lines"], 10);
        assert_eq!(first["returned_lines"], 10);
        assert_eq!(
            first["output"].as_str().unwrap().replace("\r\n", "\n"),
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n"
        );

        let checkout = session
            .checkout(&Map::from_iter([("tail_line_num".to_string(), json!(3))]))
            .await
            .unwrap();

        assert_eq!(checkout["total_lines"], 10);
        assert_eq!(checkout["returned_lines"], 3);
        assert_eq!(
            checkout["output"].as_str().unwrap().replace("\r\n", "\n"),
            "line8\nline9\nline10\n"
        );
    }

    #[tokio::test]
    async fn terminal_wait_sleeps_for_requested_time() {
        let started = Instant::now();

        let output = terminal_wait(0.1).await.unwrap();

        assert_eq!(output["status"], "ok");
        assert_eq!(output["done"], true);
        assert_eq!(output["waited_seconds"], 0.1);
        assert!(started.elapsed() >= Duration::from_millis(90));
    }
}
