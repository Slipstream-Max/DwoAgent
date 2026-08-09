use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Serialize, Serializer};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use super::session::{SessionSnapshot, SessionStatus, TerminalSession};
use crate::ToolEventHandler;

/// How long a finished terminal stays listed before it is pruned, so the
/// model has time to poll any remaining output.
const FINISHED_RETENTION: Duration = Duration::from_secs(5 * 60);
/// Hard cap on tracked terminals; overflow evicts the oldest finished ones.
const TERMINAL_CAP: usize = 64;

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
    session: Arc<TerminalSession>,
    operation: Mutex<()>,
    // Interior mutability: entries are shared behind `Arc` with the map.
    finished_since: std::sync::Mutex<Option<Instant>>,
}

pub(crate) struct TerminalTelemetry {
    tool_call_id: String,
    events: Option<ToolEventHandler>,
}

impl TerminalTelemetry {
    pub(crate) fn new(tool_call_id: String, events: Option<ToolEventHandler>) -> Self {
        Self {
            tool_call_id,
            events,
        }
    }
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
}

impl TerminalManager {
    pub fn new(base_cwd: impl Into<PathBuf>) -> Result<Self> {
        Self::new_with_environment(base_cwd, HashMap::new())
    }

    pub fn new_with_environment(
        base_cwd: impl Into<PathBuf>,
        environment: HashMap<String, String>,
    ) -> Result<Self> {
        // dunce strips the Windows verbatim prefix (`\\?\`) that
        // std::fs::canonicalize always adds; children (notably cmd.exe)
        // cannot use verbatim/UNC paths as their working directory.
        let base_cwd = dunce::canonicalize(base_cwd.into())?;
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
        yield_ms: u64,
        timeout_ms: u64,
    ) -> Result<TerminalSnapshot> {
        self.run_with_events(command, cwd, yield_ms, timeout_ms, None)
            .await
    }

    pub(crate) async fn run_with_events(
        &self,
        command: String,
        cwd: Option<&Path>,
        yield_ms: u64,
        timeout_ms: u64,
        telemetry: Option<TerminalTelemetry>,
    ) -> Result<TerminalSnapshot> {
        let (tool_call_id, events) = telemetry
            .map(|telemetry| (telemetry.tool_call_id, telemetry.events))
            .unwrap_or_default();
        let cwd = resolve_cwd(&self.base_cwd, cwd)?;
        let terminal_id = TerminalId::new();
        let session = TerminalSession::spawn(
            terminal_id.clone(),
            tool_call_id,
            command,
            cwd,
            &self.environment,
            events,
        )
        .await?;
        let entry = Arc::new(TerminalEntry {
            session: session.clone(),
            operation: Mutex::new(()),
            finished_since: std::sync::Mutex::new(None),
        });
        {
            let mut terminals = self.terminals.write().await;
            terminals.insert(terminal_id, entry.clone());
            prune(&mut terminals);
        }

        let timeout_session = session.clone();
        tokio::spawn(async move {
            let timed_out = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => true,
                // Do not keep the process (and its PTY/output buffers) alive
                // for the full timeout after a short-lived command exits.
                _ = timeout_session.wait_for_exit(timeout_ms.saturating_add(1)) => false,
            };
            if timed_out && timeout_session.is_running() {
                let _ = timeout_session.kill().await;
            }
        });

        let _operation = entry.operation.lock().await;
        session.wait_for_exit(yield_ms).await;
        Ok(render_snapshot(&session, session.snapshot()))
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
            entry.session.input(data).await?;
        }
        entry.session.wait_for_activity(yield_ms).await;
        self.prune_terminals().await;
        Ok(render_snapshot(&entry.session, entry.session.snapshot()))
    }

    pub async fn kill(&self, terminal_id: &TerminalId) -> Result<TerminalSnapshot> {
        let entry = self.entry(terminal_id).await?;
        let _operation = entry.operation.lock().await;
        entry.session.kill().await?;
        entry.session.wait_for_exit(2_000).await;
        self.prune_terminals().await;
        Ok(render_snapshot(&entry.session, entry.session.snapshot()))
    }

    pub async fn list(&self) -> Vec<TerminalSnapshot> {
        self.prune_terminals().await;
        let entries: Vec<_> = self.terminals.read().await.values().cloned().collect();
        entries
            .into_iter()
            .map(|entry| render_snapshot(&entry.session, entry.session.inspect_snapshot()))
            .collect()
    }

    pub async fn shutdown_all(&self) {
        let entries: Vec<_> = self.terminals.read().await.values().cloned().collect();
        for entry in entries {
            let _operation = entry.operation.lock().await;
            let _ = entry.session.kill().await;
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

    async fn prune_terminals(&self) {
        let mut terminals = self.terminals.write().await;
        prune(&mut terminals);
    }
}

/// Drop finished terminals whose output has had time to be polled, and cap
/// the total so long sessions cannot accumulate unbounded buffers. Running
/// terminals are never evicted; a terminal removed here simply reports
/// "terminal not found" on later calls, matching the tool contract.
fn prune(terminals: &mut HashMap<TerminalId, Arc<TerminalEntry>>) {
    let now = Instant::now();
    // Stamp the finish time on first observation after exit.
    for entry in terminals.values() {
        if let Ok(mut since) = entry.finished_since.lock()
            && since.is_none()
            && entry.session.is_finished()
        {
            *since = Some(now);
        }
    }
    let mut removable: Vec<TerminalId> = terminals
        .iter()
        .filter(|(_, entry)| {
            entry
                .finished_since
                .lock()
                .ok()
                .and_then(|guard| *guard)
                .is_some_and(|since| now.duration_since(since) >= FINISHED_RETENTION)
        })
        .map(|(id, _)| id.clone())
        .collect();
    let live = terminals.len() - removable.len();
    if live > TERMINAL_CAP {
        // Evict finished terminals beyond the cap, preferring entries whose
        // output has already been consumed, then oldest first.
        let mut candidates: Vec<(TerminalId, Instant, bool)> = terminals
            .iter()
            .filter(|(id, _)| !removable.contains(id))
            .filter_map(|(id, entry)| {
                entry
                    .finished_since
                    .lock()
                    .ok()
                    .and_then(|guard| *guard)
                    .map(|since| (id.clone(), since, entry.session.has_unread()))
            })
            .collect();
        candidates.sort_by_key(|(_, since, has_unread)| (*has_unread, *since));
        let to_evict = live - TERMINAL_CAP;
        for (id, _, _) in candidates.into_iter().take(to_evict) {
            removable.push(id);
        }
    }
    for id in removable {
        terminals.remove(&id);
    }
}

fn render_snapshot(session: &TerminalSession, snapshot: SessionSnapshot) -> TerminalSnapshot {
    let status = match snapshot.status {
        SessionStatus::Running => "running",
        SessionStatus::Killed => "cancelled",
        SessionStatus::Exited if snapshot.exit_code == Some(0) => "completed",
        SessionStatus::Exited => "error",
    };
    TerminalSnapshot {
        terminal_id: session.id.clone(),
        status,
        exit_code: snapshot.exit_code,
        output: snapshot.output,
        command: session.command.clone(),
        cwd: session.cwd.clone(),
    }
}

fn resolve_cwd(base: &Path, requested: Option<&Path>) -> Result<PathBuf> {
    let cwd = requested
        .map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                base.join(path)
            }
        })
        .unwrap_or_else(|| base.to_path_buf());
    let cwd = dunce::canonicalize(&cwd)
        .with_context(|| format!("resolve terminal cwd {}", cwd.display()))?;
    if !cwd.is_dir() {
        bail!("terminal cwd is not a directory: {}", cwd.display());
    }
    Ok(cwd)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::ToolEvent;

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
            .run(output_command().to_string(), None, 5_000, 120_000)
            .await
            .unwrap();
        assert_eq!(snapshot.status, "completed");
        assert!(snapshot.output.contains("done"));
    }

    #[tokio::test]
    async fn exit_drains_output_without_a_trailing_newline() {
        let manager = TerminalManager::new(std::env::current_dir().unwrap()).unwrap();
        let command = if cfg!(windows) {
            "[Console]::Out.Write('tail-without-newline')"
        } else {
            "printf 'tail-without-newline'"
        };
        let snapshot = manager
            .run(command.to_string(), None, 5_000, 120_000)
            .await
            .unwrap();
        assert_eq!(snapshot.status, "completed");
        assert!(snapshot.output.contains("tail-without-newline"));
    }

    #[tokio::test]
    async fn output_larger_than_the_pty_channel_does_not_stall() {
        let manager = TerminalManager::new(std::env::current_dir().unwrap()).unwrap();
        let command = if cfg!(windows) {
            "[Console]::Out.Write(('x' * 1200000) + 'END-MARKER')"
        } else {
            "head -c 1200000 /dev/zero | tr '\\0' x; printf 'END-MARKER'"
        };
        let snapshot = manager
            .run(command.to_string(), None, 15_000, 120_000)
            .await
            .unwrap();
        assert_eq!(snapshot.status, "completed");
        assert!(snapshot.output.contains("output omitted"));
        assert!(snapshot.output.contains("END-MARKER"));
    }

    #[tokio::test]
    async fn telemetry_streams_open_output_and_exit_in_order() {
        let manager = TerminalManager::new(std::env::current_dir().unwrap()).unwrap();
        let recorded = Arc::new(StdMutex::new(Vec::new()));
        let events: ToolEventHandler = Arc::new({
            let recorded = recorded.clone();
            move |event| recorded.lock().unwrap().push(event)
        });

        manager
            .run_with_events(
                output_command().to_string(),
                None,
                5_000,
                120_000,
                Some(TerminalTelemetry::new("call-1".to_string(), Some(events))),
            )
            .await
            .unwrap();

        let events = recorded.lock().unwrap();
        assert!(events.len() >= 3, "incomplete telemetry: {events:?}");
        let (terminal_id, cwd) = match &events[0] {
            ToolEvent::TerminalOpened {
                tool_call_id,
                terminal_id,
                cwd,
                ..
            } => {
                assert_eq!(tool_call_id, "call-1");
                (terminal_id, cwd)
            }
            event => panic!("first event was not terminal_opened: {event:?}"),
        };
        assert!(cwd.is_absolute());
        let output = events[1..events.len() - 1]
            .iter()
            .flat_map(|event| match event {
                ToolEvent::TerminalOutput {
                    terminal_id: output_id,
                    data,
                } => {
                    assert_eq!(output_id, terminal_id);
                    data.iter()
                }
                event => panic!("unexpected event between open and exit: {event:?}"),
            })
            .copied()
            .collect::<Vec<_>>();
        assert!(String::from_utf8_lossy(&output).contains("done"));
        assert!(matches!(
            events.last(),
            Some(ToolEvent::TerminalExited {
                terminal_id: exit_id,
                status,
                ..
            }) if exit_id == terminal_id && status == "completed"
        ));
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
            .run(command.to_string(), None, 5_000, 120_000)
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
            .run(command.to_string(), None, 500, 10_000)
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
            .run(command.to_string(), None, 5_000, 120_000)
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
            .run(command.to_string(), None, 100, 120_000)
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
