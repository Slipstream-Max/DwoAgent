//! Terminal executor and session implementation.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};

use super::session::{Cap, ToolArgs, ToolSession};

const SLICE_MODES: &[&str] = &["head", "tail", "startwith"];

// ── Handle ─────────────────────────────────────────────────────────────────

/// One terminal process handle. Mirror of Python's `TerminalHandle`.
pub struct TerminalHandle {
    /// Kept alive so the child is reaped when the handle drops even if the
    /// watcher task exits early; the field itself is written only during
    /// spawn and shared with the watcher thread.
    #[allow(dead_code)]
    child: Arc<Mutex<Option<Child>>>,
    state: Arc<Mutex<HandleState>>,
    /// Mirrors the subset of `HandleState` needed by synchronous callers
    /// (`list_item`, `is_done`) so they don't have to grab the async lock.
    atomic_state: Arc<AtomicHandleState>,
    done: Arc<Notify>,
    pid: Option<u32>,
}

/// Lock-free view over the handle. Mirrors Python's `threading.Event` +
/// public attribute reads.
#[derive(Default)]
struct AtomicHandleState {
    finished: std::sync::atomic::AtomicBool,
    killed: std::sync::atomic::AtomicBool,
    /// `i64` so we can encode `None` as a sentinel. We stash `i32::MIN as i64`
    /// when the exit code is missing.
    exit_code: std::sync::atomic::AtomicI64,
}

const EXIT_NONE: i64 = i64::MIN;

impl AtomicHandleState {
    fn new() -> Self {
        Self {
            finished: std::sync::atomic::AtomicBool::new(false),
            killed: std::sync::atomic::AtomicBool::new(false),
            exit_code: std::sync::atomic::AtomicI64::new(EXIT_NONE),
        }
    }

    fn finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::Acquire)
    }

    fn killed(&self) -> bool {
        self.killed.load(std::sync::atomic::Ordering::Acquire)
    }

    fn exit_code(&self) -> Option<i32> {
        let raw = self.exit_code.load(std::sync::atomic::Ordering::Acquire);
        if raw == EXIT_NONE {
            None
        } else {
            Some(raw as i32)
        }
    }
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
}

// ── Executor ───────────────────────────────────────────────────────────────

/// Low-level terminal operations. Does not own run ids.
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
        #[cfg(unix)]
        {
            // Mirror Python's `start_new_session=True` on POSIX so the child
            // becomes its own process group for kill-tree purposes.
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let state = Arc::new(Mutex::new(HandleState::new()));
        let atomic_state = Arc::new(AtomicHandleState::new());
        let done = Arc::new(Notify::new());

        if let Some(stdout) = stdout {
            Self::spawn_reader(stdout, state.clone());
        }
        if let Some(stderr) = stderr {
            Self::spawn_reader(stderr, state.clone());
        }

        let child_arc = Arc::new(Mutex::new(Some(child)));
        Self::spawn_watcher(
            child_arc.clone(),
            state.clone(),
            atomic_state.clone(),
            done.clone(),
        );

        Ok(TerminalHandle {
            child: child_arc,
            state,
            atomic_state,
            done,
            pid,
        })
    }

    /// Wait for process completion. Returns `true` if finished, `false` on
    /// timeout. Mirror of `_executor.wait(timeout=...)`.
    pub async fn wait(&self, handle: &TerminalHandle, timeout_secs: f64) -> bool {
        if handle.atomic_state.finished() {
            return true;
        }
        let timeout = Duration::from_secs_f64(positive_float(timeout_secs, 30.0));
        tokio::select! {
            _ = handle.done.notified() => true,
            _ = tokio::time::sleep(timeout) => handle.atomic_state.finished(),
        }
    }

    /// Kill the process tree if still running. Mirror of `_executor.kill`.
    pub async fn kill(&self, handle: &TerminalHandle) -> Result<()> {
        if handle.atomic_state.finished() {
            return Ok(());
        }
        {
            let mut state = handle.state.lock().await;
            state.killed = true;
        }
        handle
            .atomic_state
            .killed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(pid) = handle.pid {
            kill_process_tree(pid).await?;
        }
        Ok(())
    }

    /// Snapshot the currently buffered output with slicing.
    pub async fn snapshot_lines(
        &self,
        handle: &TerminalHandle,
        lines: i64,
        mode: &str,
        startwith: i64,
    ) -> Value {
        let output_lines = handle.state.lock().await.output_lines.clone();

        let slice_mode = normalize_mode(mode);
        let slice_startwith = positive_int(startwith, 1);
        let sliced = slice_output(
            &output_lines,
            positive_int(lines, 200),
            &slice_mode,
            slice_startwith,
        );
        json!({
            "output": sliced.concat(),
            "returned_lines": sliced.len(),
            "total_lines": output_lines.len(),
            "slice_mode": slice_mode,
            "slice_startwith": slice_startwith,
        })
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
    ) {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let mut s = state.lock().await;
                        s.output_lines.push(buf.clone());
                    }
                    Err(_) => break,
                }
            }
        });
    }

    fn spawn_watcher(
        child: Arc<Mutex<Option<Child>>>,
        state: Arc<Mutex<HandleState>>,
        atomic_state: Arc<AtomicHandleState>,
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
            let exit_code = status.and_then(|s| s.code());
            {
                let mut s = state.lock().await;
                s.exit_code = exit_code;
                s.finished = true;
            }
            atomic_state.exit_code.store(
                exit_code.map(i64::from).unwrap_or(EXIT_NONE),
                std::sync::atomic::Ordering::Release,
            );
            atomic_state
                .finished
                .store(true, std::sync::atomic::Ordering::Release);
            done.notify_waiters();
        });
    }
}

// ── Session ────────────────────────────────────────────────────────────────

/// One terminal command run. Mirror of Python's `TerminalSession`.
pub struct TerminalSession {
    session_id: String,
    executor: TerminalExecutor,
    command: String,
    env: Option<HashMap<String, String>>,
    timeout: f64,
    lines: i64,
    mode: String,
    startwith: i64,
    handle: Option<TerminalHandle>,
    final_payload: Option<Map<String, Value>>,
}

impl TerminalSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: impl Into<String>,
        executor: TerminalExecutor,
        command: impl Into<String>,
        env: Option<HashMap<String, String>>,
        timeout: f64,
        lines: i64,
        mode: impl Into<String>,
        startwith: i64,
    ) -> Self {
        let command_text = command.into().trim().to_string();
        Self {
            session_id: session_id.into(),
            executor,
            command: command_text,
            env: normalize_env(env),
            timeout: positive_float(timeout, 30.0),
            lines: positive_int(lines, 200),
            mode: normalize_mode(&mode.into()),
            startwith: positive_int(startwith, 1),
            handle: None,
            final_payload: None,
        }
    }

    /// Snapshot mirroring Python's sync `snapshot()` helper.
    pub async fn snapshot(
        &self,
        lines: Option<i64>,
        mode: Option<&str>,
        startwith: Option<i64>,
    ) -> Result<Value> {
        if let Some(payload) = &self.final_payload {
            return Ok(self.slice_final(payload.clone(), lines.unwrap_or(self.lines)));
        }
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| anyhow!("terminal session has not started"))?;
        Ok(self
            .running_snapshot(
                handle,
                lines.unwrap_or(self.lines),
                mode.unwrap_or(&self.mode),
                startwith.unwrap_or(self.startwith),
                None,
            )
            .await)
    }

    // ── Helpers ────────────────────────────────────────────────────────────

    async fn running_snapshot(
        &self,
        handle: &TerminalHandle,
        lines: i64,
        mode: &str,
        startwith: i64,
        message: Option<&str>,
    ) -> Value {
        let mut payload = Map::new();
        payload.insert("run_id".to_string(), Value::String(self.session_id.clone()));
        payload.insert("runtime".to_string(), self.runtime("running"));
        payload.insert("status".to_string(), Value::String("running".to_string()));
        payload.insert("done".to_string(), Value::Bool(false));
        let slice = self
            .executor
            .snapshot_lines(handle, lines, mode, startwith)
            .await;
        if let Value::Object(obj) = slice {
            for (k, v) in obj {
                payload.insert(k, v);
            }
        }
        if let Some(msg) = message {
            payload.insert("message".to_string(), Value::String(msg.to_string()));
        }
        Value::Object(payload)
    }

    async fn finalize(&mut self, lines: i64) -> Value {
        let handle = match &self.handle {
            Some(h) => h,
            None => return Value::Null,
        };
        let (status, exit_code) = {
            let state = handle.state.lock().await;
            let status = if state.killed {
                "killed"
            } else if state.exit_code == Some(0) {
                "completed_success"
            } else {
                "completed_error"
            };
            (status, state.exit_code)
        };

        let mut payload = Map::new();
        payload.insert("run_id".to_string(), Value::String(self.session_id.clone()));
        payload.insert("runtime".to_string(), self.runtime(status));
        payload.insert("status".to_string(), Value::String(status.to_string()));
        payload.insert("done".to_string(), Value::Bool(true));
        payload.insert(
            "exit_code".to_string(),
            exit_code
                .map(|v| Value::Number(v.into()))
                .unwrap_or(Value::Null),
        );
        let slice = self.executor.snapshot_lines(handle, lines, "tail", 1).await;
        if let Value::Object(obj) = slice {
            for (k, v) in obj {
                payload.insert(k, v);
            }
        }
        payload.insert("slice_mode".to_string(), Value::String("tail".to_string()));

        self.final_payload = Some(payload.clone());
        Value::Object(payload)
    }

    fn slice_final(&self, mut payload: Map<String, Value>, lines: i64) -> Value {
        let output = payload
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let output_lines: Vec<String> = splitlines_keepends(&output);
        let sliced = slice_output(&output_lines, positive_int(lines, 200), "tail", 1);
        payload.insert("output".to_string(), Value::String(sliced.concat()));
        payload.insert(
            "returned_lines".to_string(),
            Value::Number(sliced.len().into()),
        );
        payload.insert("slice_mode".to_string(), Value::String("tail".to_string()));
        let status = payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string();
        payload.insert("runtime".to_string(), self.runtime(&status));
        Value::Object(payload)
    }

    fn runtime(&self, status: &str) -> Value {
        json!({
            "kind": "terminal",
            "id": self.session_id,
            "status": status,
        })
    }

    /// Matches the async shape of Python's `_list_status()`; we keep the
    /// async signature even though the underlying read is lock-free so
    /// future extensions (e.g. awaiting buffered state) stay source-compatible.
    #[allow(dead_code)]
    async fn list_status(&self) -> String {
        self.list_status_sync()
    }

    fn list_status_sync(&self) -> String {
        if let Some(payload) = &self.final_payload {
            return payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed")
                .to_string();
        }
        let Some(handle) = &self.handle else {
            return "created".to_string();
        };
        if !handle.atomic_state.finished() {
            return "running".to_string();
        }
        if handle.atomic_state.killed() {
            return "killed".to_string();
        }
        if handle.atomic_state.exit_code() == Some(0) {
            "completed_success".to_string()
        } else {
            "completed_error".to_string()
        }
    }
}

#[async_trait]
impl ToolSession for TerminalSession {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn capabilities(&self) -> HashSet<Cap> {
        let mut caps = HashSet::new();
        caps.insert(Cap::Wait);
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
                    "run_id": self.session_id,
                    "runtime": self.runtime("completed_error"),
                    "status": "completed_error",
                    "done": true,
                    "error": exc.to_string(),
                }));
            }
        }
        self.wait(self.timeout, _args).await
    }

    async fn wait(&mut self, timeout_secs: f64, args: &ToolArgs) -> Result<Value> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| anyhow!("terminal session has not started"))?;
        let finished = self
            .executor
            .wait(handle, positive_float(timeout_secs, 30.0))
            .await;
        let lines = args
            .get("lines")
            .and_then(|v| v.as_i64())
            .unwrap_or(self.lines);
        if finished {
            return Ok(self.finalize(lines).await);
        }
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.mode)
            .to_string();
        let startwith = args
            .get("startwith")
            .and_then(|v| v.as_i64())
            .unwrap_or(self.startwith);
        let handle = self.handle.as_ref().unwrap();
        Ok(self
            .running_snapshot(
                handle,
                lines,
                &mode,
                startwith,
                Some("command still running after timeout"),
            )
            .await)
    }

    async fn checkout(&mut self, args: &ToolArgs) -> Result<Value> {
        let lines = args
            .get("lines")
            .and_then(|v| v.as_i64())
            .unwrap_or(self.lines);
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.mode)
            .to_string();
        let startwith = args
            .get("startwith")
            .and_then(|v| v.as_i64())
            .unwrap_or(self.startwith);

        if let Some(payload) = &self.final_payload {
            return Ok(self.slice_final(payload.clone(), lines));
        }
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| anyhow!("terminal session has not started"))?;
        if handle.atomic_state.finished() {
            return Ok(self.finalize(lines).await);
        }
        let handle = self.handle.as_ref().unwrap();
        Ok(self
            .running_snapshot(handle, lines, &mode, startwith, None)
            .await)
    }

    async fn cancel(&mut self) -> Result<()> {
        if self.final_payload.is_some() {
            return Ok(());
        }
        let Some(handle) = &self.handle else {
            return Ok(());
        };
        let _ = self.executor.kill(handle).await;
        let _ = self.executor.wait(handle, 2.0).await;
        let _ = self.finalize(self.lines).await;
        Ok(())
    }

    fn is_done(&self) -> bool {
        if self.final_payload.is_some() {
            return true;
        }
        self.handle
            .as_ref()
            .map(|h| h.atomic_state.finished())
            .unwrap_or(false)
    }

    fn list_item(&self) -> Value {
        let status = self.list_status_sync();
        json!({
            "id": self.session_id,
            "kind": "terminal",
            "status": status,
            "done": self.is_done(),
        })
    }
}

// ── Free helpers ───────────────────────────────────────────────────────────

pub fn terminal_not_found(run_id: &str) -> Value {
    json!({
        "run_id": run_id,
        "runtime": {
            "kind": "terminal",
            "id": run_id,
            "status": "not_found",
        },
        "status": "not_found",
        "done": true,
    })
}

fn positive_int(value: i64, default: i64) -> i64 {
    if value <= 0 {
        default.max(1)
    } else {
        value.max(1)
    }
}

fn positive_float(value: f64, default: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        default.max(0.1)
    } else {
        value.max(0.1)
    }
}

fn normalize_mode(mode: &str) -> String {
    let trimmed = mode.trim().to_ascii_lowercase();
    if SLICE_MODES.iter().any(|m| *m == trimmed) {
        trimmed
    } else {
        "tail".to_string()
    }
}

fn normalize_env(env: Option<HashMap<String, String>>) -> Option<HashMap<String, String>> {
    env.map(|m| m.into_iter().map(|(k, v)| (k, v)).collect())
}

fn slice_output(output_lines: &[String], lines: i64, mode: &str, startwith: i64) -> Vec<String> {
    if output_lines.is_empty() {
        return Vec::new();
    }
    let limit = (lines.max(1)) as usize;
    match mode {
        "head" => output_lines.iter().take(limit).cloned().collect(),
        "startwith" => {
            let start = (startwith.max(1) as usize).saturating_sub(1);
            output_lines
                .iter()
                .skip(start)
                .take(limit)
                .cloned()
                .collect()
        }
        _ => {
            let skip = output_lines.len().saturating_sub(limit);
            output_lines.iter().skip(skip).cloned().collect()
        }
    }
}

fn splitlines_keepends(text: &str) -> Vec<String> {
    // Python's `str.splitlines(keepends=True)` keeps the terminator on each
    // line. Walk the string and emit on `\n`, `\r\n`, or `\r`.
    let mut out: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    let mut line_start = 0;
    while i < n {
        let b = bytes[i];
        if b == b'\n' {
            out.push(text[line_start..=i].to_string());
            i += 1;
            line_start = i;
        } else if b == b'\r' {
            let end = if i + 1 < n && bytes[i + 1] == b'\n' {
                i + 2
            } else {
                i + 1
            };
            out.push(text[line_start..end].to_string());
            i = end;
            line_start = i;
        } else {
            i += 1;
        }
    }
    if line_start < n {
        out.push(text[line_start..].to_string());
    }
    out
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

async fn kill_process_tree(pid: u32) -> Result<()> {
    if cfg!(windows) {
        let output = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            bail!("taskkill failed for process tree rooted at pid {pid}");
        }
        Ok(())
    } else {
        #[cfg(unix)]
        unsafe {
            let ret = libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            if ret != 0 {
                return Err(anyhow!(
                    "killpg failed for pid {pid}: errno={}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn quick_command() -> &'static str {
        if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        }
    }

    #[tokio::test]
    async fn wait_after_process_already_finished_returns_immediately() {
        let executor = TerminalExecutor::new(None);
        let handle = executor.exec(quick_command(), None).await.unwrap();

        assert!(executor.wait(&handle, 5.0).await);

        let started = Instant::now();
        assert!(executor.wait(&handle, 5.0).await);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "second wait should observe the completed state without waiting for timeout"
        );
    }

    #[tokio::test]
    async fn terminal_session_start_returns_completed_success() {
        let executor = TerminalExecutor::new(None);
        let mut session = TerminalSession::new(
            "terminal-test",
            executor,
            quick_command(),
            None,
            5.0,
            20,
            "tail",
            1,
        );

        let output = session.start(&Map::new()).await.unwrap();
        assert_eq!(output["status"], "completed_success");
        assert_eq!(output["done"], true);
        assert_eq!(output["output"].as_str().unwrap().trim_end(), "done");
    }
}
