use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use dwo_pty::{ProcessHandle, SpawnedProcess, TerminalSize};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;

use super::{OutputBuffer, TerminalId, environment};
use crate::{ToolEvent, ToolEventHandler};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessStatus {
    Running,
    Exited,
    Killed,
}

struct ProcessState {
    output: OutputBuffer,
    status: ProcessStatus,
    finished: bool,
    exit_code: Option<i32>,
}

pub(super) struct TerminalProcess {
    pub id: TerminalId,
    pub command: String,
    pub cwd: std::path::PathBuf,
    events: Option<ToolEventHandler>,
    state: Arc<Mutex<ProcessState>>,
    output_notify: Arc<Notify>,
    state_notify: Arc<Notify>,
    session: Arc<ProcessHandle>,
}

impl TerminalProcess {
    pub async fn spawn(
        id: TerminalId,
        tool_call_id: String,
        command: String,
        cwd: std::path::PathBuf,
        environment_overrides: &HashMap<String, String>,
        events: Option<ToolEventHandler>,
    ) -> Result<Arc<Self>> {
        let (program, args) = shell_command(&command);
        let mut env = environment::current();
        env.extend(environment_overrides.clone());
        let spawned = dwo_pty::spawn_pty_process(
            &program,
            &args,
            &cwd,
            &env,
            &None,
            TerminalSize {
                rows: 24,
                cols: 120,
            },
        )
        .await?;
        Ok(Self::from_spawned(
            id,
            tool_call_id,
            command,
            cwd,
            spawned,
            events,
        ))
    }

    fn from_spawned(
        id: TerminalId,
        tool_call_id: String,
        command: String,
        cwd: std::path::PathBuf,
        spawned: SpawnedProcess,
        events: Option<ToolEventHandler>,
    ) -> Arc<Self> {
        let SpawnedProcess {
            session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = spawned;
        let process = Arc::new(Self {
            id,
            command,
            cwd,
            events: events.clone(),
            state: Arc::new(Mutex::new(ProcessState {
                output: OutputBuffer::default(),
                status: ProcessStatus::Running,
                finished: false,
                exit_code: None,
            })),
            output_notify: Arc::new(Notify::new()),
            state_notify: Arc::new(Notify::new()),
            session: Arc::new(session),
        });
        if let Some(events) = events {
            events(ToolEvent::TerminalOpened {
                tool_call_id,
                terminal_id: process.id.to_string(),
                command: process.command.clone(),
                cwd: process.cwd.clone(),
            });
        }
        let readers = vec![
            spawn_reader(stdout_rx, process.clone()),
            spawn_reader(stderr_rx, process.clone()),
        ];
        spawn_waiter(process.clone(), exit_rx, readers);
        process
    }

    pub async fn write(&self, data: &str) -> Result<()> {
        if lock(&self.state).status != ProcessStatus::Running {
            bail!("terminal is not running");
        }
        if data.is_empty() {
            return Ok(());
        }
        self.session
            .writer_sender()
            .send(data.as_bytes().to_vec())
            .await
            .context("write terminal input")
    }

    pub async fn kill(&self) -> Result<()> {
        {
            let mut state = lock(&self.state);
            if state.finished || state.status == ProcessStatus::Killed {
                return Ok(());
            }
            state.status = ProcessStatus::Killed;
        }
        self.state_notify.notify_one();
        self.session.request_terminate();
        Ok(())
    }

    pub async fn wait_for_activity(&self, yield_ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(yield_ms.max(1));
        loop {
            {
                let state = lock(&self.state);
                if state.output.has_unread() || state.finished {
                    return;
                }
            }
            tokio::select! {
                _ = self.output_notify.notified() => {},
                _ = self.state_notify.notified() => {},
                _ = tokio::time::sleep_until(deadline) => return,
            }
        }
    }

    pub async fn wait_for_exit(&self, yield_ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(yield_ms.max(1));
        loop {
            if lock(&self.state).finished {
                return;
            }
            tokio::select! {
                _ = self.state_notify.notified() => {},
                _ = tokio::time::sleep_until(deadline) => return,
            }
        }
    }

    pub fn snapshot(&self) -> ProcessSnapshot {
        let mut state = lock(&self.state);
        ProcessSnapshot {
            status: state.status,
            exit_code: state.exit_code,
            output: state.output.take_unread(),
        }
    }

    pub fn inspect_snapshot(&self) -> ProcessSnapshot {
        let state = lock(&self.state);
        ProcessSnapshot {
            status: state.status,
            exit_code: state.exit_code,
            output: state.output.render_all(),
        }
    }

    pub fn is_running(&self) -> bool {
        !lock(&self.state).finished
    }

    pub fn is_finished(&self) -> bool {
        lock(&self.state).finished
    }

    pub fn has_unread(&self) -> bool {
        lock(&self.state).output.has_unread()
    }

    fn push_output(&self, bytes: &[u8]) {
        let clean = strip_ansi(bytes);
        // PTY startup emits blank scroll rows (CRLF runs) that are noise;
        // skip whitespace-only chunks so they neither pollute output nor
        // satisfy a wait prematurely.
        if clean.iter().all(|byte| byte.is_ascii_whitespace()) {
            return;
        }
        lock(&self.state).output.push(&clean);
        if let Some(events) = &self.events {
            events(ToolEvent::TerminalOutput {
                terminal_id: self.id.to_string(),
                data: clean.clone(),
            });
        }
        self.output_notify.notify_one();
    }

    fn mark_exited(&self, exit_code: Option<i32>) {
        let mut state = lock(&self.state);
        if state.status == ProcessStatus::Running {
            state.status = ProcessStatus::Exited;
        }
        state.finished = true;
        state.exit_code = exit_code;
        let status = match state.status {
            ProcessStatus::Killed => "cancelled",
            ProcessStatus::Exited if exit_code == Some(0) => "completed",
            ProcessStatus::Exited => "error",
            ProcessStatus::Running => "error",
        };
        drop(state);
        if let Some(events) = &self.events {
            events(ToolEvent::TerminalExited {
                terminal_id: self.id.to_string(),
                exit_code,
                status: status.to_string(),
            });
        }
        self.state_notify.notify_one();
    }
}

pub(super) struct ProcessSnapshot {
    pub status: ProcessStatus,
    pub exit_code: Option<i32>,
    pub output: String,
}

fn spawn_reader(
    mut receiver: mpsc::Receiver<Vec<u8>>,
    process: Arc<TerminalProcess>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(bytes) = receiver.recv().await {
            process.push_output(&bytes);
        }
    })
}

fn spawn_waiter(
    process: Arc<TerminalProcess>,
    exit_rx: oneshot::Receiver<i32>,
    readers: Vec<tokio::task::JoinHandle<()>>,
) {
    tokio::spawn(async move {
        let exit_code = exit_rx.await.ok();
        // The PTY master never EOFs on its own (ConPTY output pipes stay
        // open after the child exits), so stop the readers explicitly and
        // let the output channel close. The bounded await is a backstop so
        // exit state is still reported if a platform fails to unblock its reader.
        process.session.shutdown_readers();
        for reader in readers {
            let _ = tokio::time::timeout(Duration::from_millis(1_000), reader).await;
        }
        process.mark_exited(exit_code);
    });
}

fn shell_command(command: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "powershell.exe".to_string(),
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                // PowerShell 5.1 attached to a PTY (ConPTY) keeps running after
                // -Command finishes; an explicit `exit` is required to return.
                format!("chcp 65001 >$null;$ProgressPreference='SilentlyContinue';{command}; exit"),
            ],
        )
    } else {
        (
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("terminal process mutex poisoned")
}

/// Strip ANSI/VT escape sequences (CSI, OSC, and single-character escapes)
/// from terminal output. PTYs emit control sequences for cursor movement,
/// colors, and window title updates that are noise for the model.
///
/// Raw C1 control bytes (0x9B/0x9D) are deliberately not stripped: they fall
/// inside the UTF-8 continuation range (0x80-0xBF), so treating them as
/// control characters would corrupt multibyte CJK punctuation such as
/// U+201D or U+301D. Modern terminals emit CSI/OSC with an ESC prefix
/// anyway.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut clean = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                match bytes[i] {
                    b'[' => i = skip_csi(bytes, i + 1),
                    b']' => i = skip_osc(bytes, i + 1),
                    other if (0x20..=0x2f).contains(&other) => {
                        // Two-character escape such as ESC ( B.
                        i += 1;
                        if i < bytes.len() {
                            i += 1;
                        }
                    }
                    _ => i += 1,
                }
            }
            byte => {
                clean.push(byte);
                i += 1;
            }
        }
    }
    clean
}

fn skip_csi(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
        i += 1;
    }
    if i < bytes.len() { i + 1 } else { i }
}

fn skip_osc(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != 0x07 {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            return i + 2;
        }
        i += 1;
    }
    if i < bytes.len() && bytes[i] == 0x07 {
        i + 1
    } else {
        i
    }
}

pub(super) fn resolve_cwd(base: &Path, requested: Option<&Path>) -> Result<std::path::PathBuf> {
    let cwd = requested
        .map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                base.join(path)
            }
        })
        .unwrap_or_else(|| base.to_path_buf());
    // dunce strips the Windows verbatim prefix (`\\?\`) that
    // std::fs::canonicalize always adds; children (notably cmd.exe) cannot
    // use verbatim/UNC paths as their working directory.
    let cwd = dunce::canonicalize(&cwd)
        .with_context(|| format!("resolve terminal cwd {}", cwd.display()))?;
    if !cwd.is_dir() {
        bail!("terminal cwd is not a directory: {}", cwd.display());
    }
    Ok(cwd)
}

#[cfg(test)]
mod strip_ansi_tests {
    use super::strip_ansi;

    #[test]
    fn removes_csi_osc_and_two_character_escapes() {
        let input = b"hello\x1b[31mred\x1b[0m\x1b]0;title\x07end\x1b[K\x1b(Beof";
        let expected: Vec<u8> = b"hello"
            .iter()
            .chain(b"red")
            .chain(b"end")
            .chain(b"eof")
            .copied()
            .collect();
        assert_eq!(strip_ansi(input), expected);
    }

    #[test]
    fn preserves_plain_text_and_utf8() {
        assert_eq!(strip_ansi("你好\r\n".as_bytes()), "你好\r\n".as_bytes());
        assert_eq!(strip_ansi(b"plain output"), b"plain output");
    }

    #[test]
    fn preserves_c1_control_bytes_and_utf8_continuations() {
        // 0x9B/0x9D are also valid UTF-8 continuation bytes; they must pass
        // through untouched so CJK punctuation such as U+201D / U+301D is
        // not mistaken for an OSC sequence.
        assert_eq!(strip_ansi(b"a\x9b3mb\x9d0;c\x07d"), b"a\x9b3mb\x9d0;c\x07d");
        assert_eq!(strip_ansi("”〗".as_bytes()), "”〗".as_bytes());
    }
}
