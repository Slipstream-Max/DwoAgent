use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::{Serialize, Serializer};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use super::process::{ProcessSnapshot, ProcessStatus, TerminalProcess, resolve_cwd};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalId(String);

impl TerminalId {
    pub fn new() -> Self {
        Self(format!("term-{}", Uuid::new_v4()))
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("terminal_id must not be empty.".to_string());
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TerminalId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for TerminalId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

struct TerminalEntry {
    process: Arc<TerminalProcess>,
    operation: Mutex<()>,
}

pub struct TerminalManager {
    base_cwd: PathBuf,
    environment: HashMap<String, String>,
    terminals: RwLock<HashMap<TerminalId, Arc<TerminalEntry>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalSnapshot {
    pub terminal_id: TerminalId,
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub output: String,
    pub command: String,
    pub cwd: PathBuf,
    pub tty: bool,
    pub pid: Option<u32>,
}

impl TerminalManager {
    pub fn new(base_cwd: impl Into<PathBuf>) -> Result<Self> {
        Self::new_with_environment(base_cwd, HashMap::new())
    }

    pub fn new_with_environment(
        base_cwd: impl Into<PathBuf>,
        environment: HashMap<String, String>,
    ) -> Result<Self> {
        let base_cwd = std::fs::canonicalize(base_cwd.into())?;
        Ok(Self {
            base_cwd,
            environment,
            terminals: RwLock::new(HashMap::new()),
        })
    }

    pub async fn run(
        &self,
        command: String,
        cwd: Option<&Path>,
        tty: bool,
        yield_ms: u64,
        timeout_ms: Option<u64>,
    ) -> Result<TerminalSnapshot> {
        let cwd = resolve_cwd(&self.base_cwd, cwd)?;
        let terminal_id = TerminalId::new();
        let process =
            TerminalProcess::spawn(terminal_id.clone(), command, cwd, tty, &self.environment)
                .await?;
        let entry = Arc::new(TerminalEntry {
            process: process.clone(),
            operation: Mutex::new(()),
        });
        self.terminals
            .write()
            .await
            .insert(terminal_id, entry.clone());

        if let Some(timeout_ms) = timeout_ms {
            let process = process.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
                if process.is_running() {
                    let _ = process.kill().await;
                }
            });
        }

        let _operation = entry.operation.lock().await;
        process.wait_for_exit(yield_ms).await;
        Ok(render_snapshot(&process, process.snapshot()))
    }

    pub async fn input(
        &self,
        terminal_id: &TerminalId,
        data: &str,
        yield_ms: u64,
    ) -> Result<TerminalSnapshot> {
        let entry = self.entry(terminal_id).await?;
        let _operation = entry.operation.lock().await;
        if !data.is_empty() {
            entry.process.write(data).await?;
        }
        entry.process.wait_for_activity(yield_ms).await;
        Ok(render_snapshot(&entry.process, entry.process.snapshot()))
    }

    pub async fn kill(&self, terminal_id: &TerminalId) -> Result<TerminalSnapshot> {
        let entry = self.entry(terminal_id).await?;
        let _operation = entry.operation.lock().await;
        entry.process.kill().await?;
        entry.process.wait_for_exit(2_000).await;
        Ok(render_snapshot(&entry.process, entry.process.snapshot()))
    }

    pub async fn list(&self) -> Vec<TerminalSnapshot> {
        let entries: Vec<_> = self.terminals.read().await.values().cloned().collect();
        entries
            .into_iter()
            .map(|entry| render_snapshot(&entry.process, entry.process.inspect_snapshot()))
            .collect()
    }

    pub async fn shutdown_all(&self) {
        let entries: Vec<_> = self.terminals.read().await.values().cloned().collect();
        for entry in entries {
            let _operation = entry.operation.lock().await;
            let _ = entry.process.kill().await;
        }
    }

    async fn entry(&self, terminal_id: &TerminalId) -> Result<Arc<TerminalEntry>> {
        self.terminals
            .read()
            .await
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| anyhow!("terminal not found: {terminal_id}"))
    }
}

fn render_snapshot(process: &TerminalProcess, snapshot: ProcessSnapshot) -> TerminalSnapshot {
    let status = match snapshot.status {
        ProcessStatus::Running => "running",
        ProcessStatus::Killed => "cancelled",
        ProcessStatus::Exited if snapshot.exit_code == Some(0) => "completed",
        ProcessStatus::Exited => "error",
    };
    TerminalSnapshot {
        terminal_id: process.id.clone(),
        status,
        exit_code: snapshot.exit_code,
        output: snapshot.output,
        command: process.command.clone(),
        cwd: process.cwd.clone(),
        tty: process.tty,
        pid: process.pid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_command() -> &'static str {
        if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        }
    }

    #[tokio::test]
    async fn run_returns_output_and_status() {
        let manager = TerminalManager::new(std::env::current_dir().unwrap()).unwrap();
        let snapshot = manager
            .run(output_command().to_string(), None, false, 5_000, None)
            .await
            .unwrap();
        assert_eq!(snapshot.status, "completed");
        assert!(snapshot.output.contains("done"));
    }

    #[tokio::test]
    async fn empty_input_polls_incrementally() {
        let manager = TerminalManager::new(std::env::current_dir().unwrap()).unwrap();
        let command = if cfg!(windows) {
            "Write-Output first; Start-Sleep -Milliseconds 1500; Write-Output second"
        } else {
            "printf 'first\\n'; sleep 1.5; printf 'second\\n'"
        };
        let first = manager
            .run(command.to_string(), None, false, 5_000, None)
            .await
            .unwrap();
        assert!(first.output.contains("first"));
        let second = manager.input(&first.terminal_id, "", 3_000).await.unwrap();
        assert!(second.output.contains("second") || second.status == "completed");
        assert!(!second.output.contains("first"));
    }

    #[tokio::test]
    async fn pty_input_reaches_the_process() {
        let manager = TerminalManager::new(std::env::current_dir().unwrap()).unwrap();
        let command = if cfg!(windows) {
            "powershell.exe -NoLogo -NoProfile -NoExit"
        } else {
            "sh -i"
        };
        let started = manager
            .run(command.to_string(), None, true, 500, Some(10_000))
            .await
            .unwrap();
        assert_eq!(
            started.status, "running",
            "exit={:?}, output={}",
            started.exit_code, started.output
        );
        let input = if cfg!(windows) {
            "Write-Output ([string][char]80 + [char]84 + [char]89 + '-' + [char]79 + [char]75)\n"
        } else {
            "printf '\\120\\124\\131\\055\\117\\113\\n'\n"
        };
        let mut output = manager
            .input(&started.terminal_id, input, 1_000)
            .await
            .unwrap()
            .output;
        for _ in 0..10 {
            if output.contains("PTY-OK") {
                break;
            }
            output.push_str(
                &manager
                    .input(&started.terminal_id, "", 500)
                    .await
                    .unwrap()
                    .output,
            );
        }
        assert!(output.contains("PTY-OK"), "{output}");
        let _ = manager.kill(&started.terminal_id).await;
    }

    #[tokio::test]
    async fn kill_returns_cancelled_and_drains_output() {
        let manager = TerminalManager::new(std::env::current_dir().unwrap()).unwrap();
        let command = if cfg!(windows) {
            "Write-Output before-kill; Start-Sleep -Seconds 30"
        } else {
            "printf 'before-kill\\n'; sleep 30"
        };
        let started = manager
            .run(command.to_string(), None, false, 5_000, None)
            .await
            .unwrap();
        assert!(started.output.contains("before-kill"));
        let killed = manager.kill(&started.terminal_id).await.unwrap();
        assert_eq!(killed.status, "cancelled");
    }

    #[tokio::test]
    async fn list_does_not_consume_unread_output() {
        let manager = TerminalManager::new(std::env::current_dir().unwrap()).unwrap();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 500; Write-Output later"
        } else {
            "sleep 0.5; printf 'later\\n'"
        };
        let started = manager
            .run(command.to_string(), None, false, 100, None)
            .await
            .unwrap();
        let _ = manager.list().await;
        let polled = manager
            .input(&started.terminal_id, "", 3_000)
            .await
            .unwrap();
        assert!(polled.output.contains("later"));
    }
}
