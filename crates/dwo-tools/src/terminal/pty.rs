use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use dwo_pty::{ProcessHandle, SpawnedProcess, TerminalSize};
use tokio::sync::{mpsc, oneshot};

use super::environment;

pub(crate) struct PtyProcess {
    handle: ProcessHandle,
}

pub(crate) struct SpawnedPty {
    pub process: PtyProcess,
    pub stdout_rx: mpsc::Receiver<Vec<u8>>,
    pub stderr_rx: mpsc::Receiver<Vec<u8>>,
    pub exit_rx: oneshot::Receiver<i32>,
}

impl PtyProcess {
    pub async fn spawn(
        command: &str,
        cwd: &Path,
        environment_overrides: &HashMap<String, String>,
    ) -> Result<SpawnedPty> {
        let (program, args) = shell_command(command);
        let mut environment = environment::current();
        environment.extend(environment_overrides.clone());
        let spawned = dwo_pty::spawn_pty_process(
            &program,
            &args,
            cwd,
            &environment,
            &None,
            TerminalSize {
                rows: 24,
                cols: 120,
            },
        )
        .await?;
        Ok(Self::from_spawned(spawned))
    }

    fn from_spawned(spawned: SpawnedProcess) -> SpawnedPty {
        let SpawnedProcess {
            session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = spawned;
        SpawnedPty {
            process: Self { handle: session },
            stdout_rx,
            stderr_rx,
            exit_rx,
        }
    }

    pub async fn write(&self, data: &[u8]) -> Result<()> {
        self.handle
            .writer_sender()
            .send(data.to_vec())
            .await
            .map_err(|_| anyhow::anyhow!("terminal input channel is closed"))
    }

    pub fn kill(&self) {
        self.handle.request_terminate();
    }

    pub fn shutdown_readers(&self) {
        self.handle.shutdown_readers();
    }
}

fn shell_command(command: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        let utf8 = "$utf8=New-Object System.Text.UTF8Encoding $false;[Console]::InputEncoding=$utf8;[Console]::OutputEncoding=$utf8;$global:OutputEncoding=$utf8;";
        (
            "powershell.exe".to_string(),
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                format!("chcp 65001 >$null;{utf8}$ProgressPreference='SilentlyContinue';{command}"),
            ],
        )
    } else {
        (
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }
}
