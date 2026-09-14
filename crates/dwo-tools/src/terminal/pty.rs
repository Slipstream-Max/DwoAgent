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
    pub stdout_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    pub stderr_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    pub exit_rx: oneshot::Receiver<i32>,
}

impl PtyProcess {
    pub async fn spawn(
        command: &str,
        cwd: &Path,
        environment_overrides: &HashMap<String, String>,
    ) -> Result<SpawnedPty> {
        let ShellInvocation {
            program,
            args,
            environment: shell_environment,
        } = shell_invocation(command);
        let mut environment = environment::current();
        environment.extend(environment_overrides.clone());
        // Shell defaults win over caller overrides: the model boundary decodes
        // terminal output as UTF-8, so the wrapper must guarantee it.
        environment.extend(shell_environment);
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

/// The wrapper a user command is executed through, resolved per platform.
struct ShellInvocation {
    program: String,
    args: Vec<String>,
    /// Extra environment variables the shell needs for UTF-8 output.
    environment: Vec<(String, String)>,
}

fn shell_invocation(command: &str) -> ShellInvocation {
    #[cfg(windows)]
    {
        windows_shell_invocation(command)
    }
    #[cfg(not(windows))]
    {
        ShellInvocation {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), command.to_string()],
            environment: Vec::new(),
        }
    }
}

#[cfg(windows)]
fn windows_shell_invocation(command: &str) -> ShellInvocation {
    // Keep the shell and native children on UTF-8 even when the daemon
    // inherited no locale settings.
    let utf8_environment = vec![
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
    ];
    match dwo_context::shell::Shell::detect() {
        dwo_context::shell::Shell::NiuBash(program) => ShellInvocation {
            program: program.display().to_string(),
            args: vec!["-c".to_string(), command.to_string()],
            environment: utf8_environment,
        },
        dwo_context::shell::Shell::Sh => unreachable!("Windows shell detection returned sh"),
    }
}
