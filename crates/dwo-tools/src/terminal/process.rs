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
    pub tty: bool,
    pub pid: Option<u32>,
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
        tty: bool,
        environment_overrides: &HashMap<String, String>,
        events: Option<ToolEventHandler>,
    ) -> Result<Arc<Self>> {
        let (program, args) = shell_command(&command);
        let mut env = environment::current();
        env.extend(environment_overrides.clone());
        let spawned = if tty {
            dwo_pty::spawn_pty_process(
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
            .await?
        } else {
            dwo_pty::spawn_pipe_process_no_stdin(&program, &args, &cwd, &env, &None).await?
        };
        Ok(Self::from_spawned(
            id,
            tool_call_id,
            command,
            cwd,
            tty,
            spawned,
            events,
        ))
    }

    fn from_spawned(
        id: TerminalId,
        tool_call_id: String,
        command: String,
        cwd: std::path::PathBuf,
        tty: bool,
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
            tty,
            // dwo-pty intentionally keeps the OS pid internal.
            pid: None,
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
        if !self.tty {
            bail!("stdin is closed; rerun terminal.run with tty=true to accept input");
        }
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

    fn push_output(&self, bytes: &[u8]) {
        lock(&self.state).output.push(bytes);
        if let Some(events) = &self.events {
            events(ToolEvent::TerminalOutput {
                terminal_id: self.id.to_string(),
                data: bytes.to_vec(),
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
        for reader in readers {
            let _ = reader.await;
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
                format!("chcp 65001 >$null;$ProgressPreference='SilentlyContinue';{command}"),
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
    let cwd = std::fs::canonicalize(&cwd)
        .with_context(|| format!("resolve terminal cwd {}", cwd.display()))?;
    if !cwd.is_dir() {
        bail!("terminal cwd is not a directory: {}", cwd.display());
    }
    Ok(cwd)
}
