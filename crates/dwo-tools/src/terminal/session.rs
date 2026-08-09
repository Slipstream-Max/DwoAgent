use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;

use super::pipeline::OutputPipeline;
use super::pty::PtyProcess;
use super::{OutputBuffer, TerminalId};
use crate::{ToolEvent, ToolEventHandler};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionStatus {
    Running,
    Exited,
    Killed,
}

struct SessionState {
    pipeline: OutputPipeline,
    output: OutputBuffer,
    status: SessionStatus,
    finished: bool,
    exit_code: Option<i32>,
}

pub(super) struct TerminalSession {
    pub id: TerminalId,
    pub command: String,
    pub cwd: PathBuf,
    events: Option<ToolEventHandler>,
    state: Mutex<SessionState>,
    output_notify: Notify,
    state_notify: Notify,
    pty: PtyProcess,
}

#[derive(Debug)]
pub(super) struct SessionSnapshot {
    pub status: SessionStatus,
    pub exit_code: Option<i32>,
    pub output: String,
}

impl TerminalSession {
    pub async fn spawn(
        id: TerminalId,
        tool_call_id: String,
        command: String,
        cwd: PathBuf,
        environment: &HashMap<String, String>,
        events: Option<ToolEventHandler>,
    ) -> Result<Arc<Self>> {
        let spawned = PtyProcess::spawn(&command, &cwd, environment).await?;
        let session = Arc::new(Self {
            id,
            command,
            cwd,
            events: events.clone(),
            state: Mutex::new(SessionState {
                pipeline: OutputPipeline::default(),
                output: OutputBuffer::default(),
                status: SessionStatus::Running,
                finished: false,
                exit_code: None,
            }),
            output_notify: Notify::new(),
            state_notify: Notify::new(),
            pty: spawned.process,
        });
        if let Some(events) = events {
            events(ToolEvent::TerminalOpened {
                tool_call_id,
                terminal_id: session.id.to_string(),
                command: session.command.clone(),
                cwd: session.cwd.clone(),
            });
        }
        spawn_pump(
            session.clone(),
            spawned.stdout_rx,
            spawned.stderr_rx,
            spawned.exit_rx,
        );
        Ok(session)
    }

    pub async fn input(&self, data: &str) -> Result<()> {
        if lock(&self.state).status != SessionStatus::Running {
            bail!("terminal is not running");
        }
        if !data.is_empty() {
            self.pty
                .write(data.as_bytes())
                .await
                .context("write terminal input")?;
        }
        Ok(())
    }

    pub async fn kill(&self) -> Result<()> {
        {
            let mut state = lock(&self.state);
            if state.finished || state.status == SessionStatus::Killed {
                return Ok(());
            }
            state.status = SessionStatus::Killed;
        }
        self.state_notify.notify_one();
        self.pty.kill();
        Ok(())
    }

    pub async fn wait_for_activity(&self, yield_ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(yield_ms.max(1));
        loop {
            if self.has_unread() || self.is_finished() {
                return;
            }
            tokio::select! {
                _ = self.output_notify.notified() => {}
                _ = self.state_notify.notified() => {}
                _ = tokio::time::sleep_until(deadline) => return,
            }
        }
    }

    pub async fn wait_for_exit(&self, yield_ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(yield_ms.max(1));
        loop {
            if self.is_finished() {
                return;
            }
            tokio::select! {
                _ = self.state_notify.notified() => {}
                _ = tokio::time::sleep_until(deadline) => return,
            }
        }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let mut state = lock(&self.state);
        SessionSnapshot {
            status: state.status,
            exit_code: state.exit_code,
            output: if state.finished {
                state.output.take_unread()
            } else {
                state.output.take_complete_unread()
            },
        }
    }

    pub fn inspect_snapshot(&self) -> SessionSnapshot {
        let state = lock(&self.state);
        SessionSnapshot {
            status: state.status,
            exit_code: state.exit_code,
            output: state.output.render_all(),
        }
    }

    pub fn is_running(&self) -> bool {
        !self.is_finished()
    }

    pub fn is_finished(&self) -> bool {
        lock(&self.state).finished
    }

    pub fn has_unread(&self) -> bool {
        let state = lock(&self.state);
        if state.finished {
            state.output.has_unread()
        } else {
            state.output.has_complete_unread()
        }
    }

    fn push_output(&self, bytes: &[u8]) {
        let clean = {
            let mut state = lock(&self.state);
            let clean = state.pipeline.process(bytes);
            if !clean.is_empty() {
                state.output.push(&clean);
            }
            clean
        };
        if clean.is_empty() {
            return;
        }
        if let Some(events) = &self.events {
            events(ToolEvent::TerminalOutput {
                terminal_id: self.id.to_string(),
                data: clean,
            });
        }
        self.output_notify.notify_one();
    }

    fn mark_exited(&self, exit_code: Option<i32>) {
        let (status, trailing) = {
            let mut state = lock(&self.state);
            let trailing = state.pipeline.finish();
            if !trailing.is_empty() {
                state.output.push(&trailing);
            }
            if state.status == SessionStatus::Running {
                state.status = SessionStatus::Exited;
            }
            state.finished = true;
            state.exit_code = exit_code;
            let status = match state.status {
                SessionStatus::Killed => "cancelled",
                SessionStatus::Exited if exit_code == Some(0) => "completed",
                SessionStatus::Exited => "error",
                SessionStatus::Running => "error",
            };
            (status.to_string(), trailing)
        };
        if !trailing.is_empty() {
            if let Some(events) = &self.events {
                events(ToolEvent::TerminalOutput {
                    terminal_id: self.id.to_string(),
                    data: trailing,
                });
            }
            self.output_notify.notify_one();
        }
        if let Some(events) = &self.events {
            events(ToolEvent::TerminalExited {
                terminal_id: self.id.to_string(),
                exit_code,
                status,
            });
        }
        self.state_notify.notify_one();
    }
}

fn spawn_pump(
    session: Arc<TerminalSession>,
    mut stdout_rx: mpsc::Receiver<Vec<u8>>,
    mut stderr_rx: mpsc::Receiver<Vec<u8>>,
    mut exit_rx: oneshot::Receiver<i32>,
) {
    tokio::spawn(async move {
        let mut stdout_open = true;
        let mut stderr_open = true;
        let exit_code = loop {
            tokio::select! {
                biased;
                output = stdout_rx.recv(), if stdout_open => match output {
                    Some(bytes) => session.push_output(&bytes),
                    None => stdout_open = false,
                },
                output = stderr_rx.recv(), if stderr_open => match output {
                    Some(bytes) => session.push_output(&bytes),
                    None => stderr_open = false,
                },
                result = &mut exit_rx => break result.ok(),
            }
        };
        session.pty.shutdown_readers();
        let drain = async {
            while stdout_open || stderr_open {
                tokio::select! {
                    output = stdout_rx.recv(), if stdout_open => match output {
                        Some(bytes) => session.push_output(&bytes),
                        None => stdout_open = false,
                    },
                    output = stderr_rx.recv(), if stderr_open => match output {
                        Some(bytes) => session.push_output(&bytes),
                        None => stderr_open = false,
                    },
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_millis(1_000), drain).await;
        session.mark_exited(exit_code);
    });
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("terminal session mutex poisoned")
}
