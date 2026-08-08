use std::collections::HashMap;
#[cfg(unix)]
use std::fs::File;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
#[cfg(unix)]
use std::process::Command as StdCommand;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
#[cfg(not(unix))]
use std::time::Duration;

use anyhow::Result;
#[cfg(not(unix))]
use portable_pty::CommandBuilder;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::process::ChildTerminator;
use crate::process::ProcessHandle;
use crate::process::ProcessSignal;
use crate::process::PtyHandles;
use crate::process::PtyMasterHandle;
use crate::process::ReaderStop;
use crate::process::SpawnedProcess;
use crate::process::TerminalSize;
#[cfg(unix)]
use crate::process::exit_code_from_status;

/// Returns true when ConPTY support is available (Windows only).
#[cfg(windows)]
pub fn conpty_supported() -> bool {
    crate::win::conpty_supported()
}

/// Returns true when ConPTY support is available (non-Windows always true).
#[cfg(not(windows))]
pub fn conpty_supported() -> bool {
    true
}

#[cfg(not(unix))]
struct PtyChildTerminator {
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

#[cfg(not(unix))]
impl ChildTerminator for PtyChildTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => Err(crate::process::unsupported_signal(signal)),
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.killer.kill()
    }
}

#[cfg(unix)]
struct RawPidTerminator {
    process_group_id: u32,
}

#[cfg(unix)]
impl ChildTerminator for RawPidTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => {
                crate::process_group::interrupt_process_group(self.process_group_id)
            }
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        crate::process_group::kill_process_group(self.process_group_id)
    }
}

#[cfg(not(unix))]
fn platform_native_pty_system() -> Box<dyn portable_pty::PtySystem + Send> {
    Box::new(crate::win::ConPtySystem::default())
}

/// Spawn a process attached to a PTY, returning handles for stdin, split output, and exit.
pub async fn spawn_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
) -> Result<SpawnedProcess> {
    spawn_process_with_inherited_fds(program, args, cwd, env, arg0, size, &[]).await
}

/// Spawn a process attached to a PTY, preserving any inherited file
/// descriptors listed in `inherited_fds` across exec on Unix.
pub async fn spawn_process_with_inherited_fds(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
    inherited_fds: &[i32],
) -> Result<SpawnedProcess> {
    if program.is_empty() {
        anyhow::bail!("missing program for PTY spawn");
    }

    #[cfg(unix)]
    {
        return spawn_process_unix(program, args, cwd, env, arg0, size, inherited_fds).await;
    }

    #[cfg(not(unix))]
    {
        let _ = inherited_fds;
        spawn_process_portable(program, args, cwd, env, arg0, size).await
    }
}

#[cfg(not(unix))]
async fn spawn_process_portable(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
) -> Result<SpawnedProcess> {
    let pty_system = platform_native_pty_system();
    let pair = pty_system.openpty(size.into())?;

    let mut command_builder = CommandBuilder::new(arg0.as_ref().unwrap_or(&program.to_string()));
    command_builder.cwd(cwd);
    command_builder.env_clear();
    for arg in args {
        command_builder.arg(arg);
    }
    for (key, value) in env {
        command_builder.env(key, value);
    }

    let mut child = pair.slave.spawn_command(command_builder)?;
    let killer = child.clone_killer();

    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (_stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(1);
    let mut reader = pair.master.try_clone_reader()?;
    let reader_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8_192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = stdout_tx.blocking_send(buf[..n].to_vec());
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    let writer = pair.master.take_writer()?;
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    let writer_handle: JoinHandle<()> = tokio::spawn({
        let writer = Arc::clone(&writer);
        async move {
            #[cfg(windows)]
            let mut windows_input = crate::windows_input::WindowsTtyInputNormalizer::default();
            while let Some(bytes) = writer_rx.recv().await {
                #[cfg(windows)]
                let bytes = windows_input.normalize(&bytes);
                let mut guard = writer.lock().await;
                use std::io::Write;
                let _ = guard.write_all(&bytes);
                let _ = guard.flush();
            }
        }
    });

    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let code = match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => -1,
        };
        wait_exit_status.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = wait_exit_code.lock() {
            *guard = Some(code);
        }
        let _ = exit_tx.send(code);
    });

    let handles = PtyHandles {
        _slave: Some(pair.slave),
        _master: PtyMasterHandle::Resizable(pair.master),
    };

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(PtyChildTerminator { killer }),
        reader_handle,
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        Some(handles),
        ReaderStop::None,
    );

    Ok(SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx,
    })
}

#[cfg(unix)]
async fn spawn_process_unix(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
    inherited_fds: &[RawFd],
) -> Result<SpawnedProcess> {
    let (master, slave) = open_unix_pty(size)?;
    let mut command = StdCommand::new(program);
    if let Some(arg0) = arg0 {
        command.arg0(arg0);
    }
    command.current_dir(cwd);
    command.env_clear();
    for arg in args {
        command.arg(arg);
    }
    for (key, value) in env {
        command.env(key, value);
    }

    // The child should see one terminal on all three stdio streams. Cloning
    // the slave fd gives us three owned handles to the same PTY slave device
    // so Command can wire them up independently as stdin/stdout/stderr.
    let stdin = slave.try_clone()?;
    let stdout = slave.try_clone()?;
    let stderr = slave.try_clone()?;
    let inherited_fds = inherited_fds.to_vec();

    unsafe {
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .pre_exec(move || {
                for signo in &[
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                ] {
                    libc::signal(*signo, libc::SIG_DFL);
                }

                let empty_set: libc::sigset_t = std::mem::zeroed();
                libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());

                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }

                // stdin now refers to the PTY slave, so make that fd the
                // controlling terminal for the child's new session. stdout and
                // stderr point at clones of the same slave device.
                #[allow(clippy::cast_lossless)]
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }

                close_inherited_fds_except(&inherited_fds);
                Ok(())
            });
    }

    let mut child = command.spawn()?;
    drop(slave);
    let process_group_id = child.id();

    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (_stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(1);
    let mut reader = master.try_clone()?;
    let reader_cancel = Arc::new(AtomicBool::new(false));
    let reader_handle: JoinHandle<()> = tokio::task::spawn_blocking({
        let reader_cancel = Arc::clone(&reader_cancel);
        move || {
            let fd = reader.as_raw_fd();
            let mut buf = [0u8; 8_192];
            loop {
                if reader_cancel.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let polled = unsafe { libc::poll(&mut poll_fd, 1, 50) };
                if polled < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if polled == 0 {
                    // Timed out with no data; loop re-checks the cancel flag.
                    continue;
                }
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = stdout_tx.blocking_send(buf[..n].to_vec());
                    }
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(_) => break,
                }
            }
        }
    });

    let writer = Arc::new(tokio::sync::Mutex::new(master.try_clone()?));
    let writer_handle: JoinHandle<()> = tokio::spawn({
        let writer = Arc::clone(&writer);
        async move {
            while let Some(bytes) = writer_rx.recv().await {
                let mut guard = writer.lock().await;
                use std::io::Write;
                let _ = guard.write_all(&bytes);
                let _ = guard.flush();
            }
        }
    });

    let (exit_tx, exit_rx) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle: JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let code = match child.wait() {
            Ok(status) => exit_code_from_status(status),
            Err(_) => -1,
        };
        wait_exit_status.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = wait_exit_code.lock() {
            *guard = Some(code);
        }
        let _ = exit_tx.send(code);
    });

    let handles = PtyHandles {
        _slave: None,
        _master: PtyMasterHandle::Opaque {
            raw_fd: master.as_raw_fd(),
            _handle: Box::new(master),
        },
    };

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(RawPidTerminator { process_group_id }),
        reader_handle,
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        Some(handles),
        ReaderStop::PollFlag(reader_cancel),
    );

    Ok(SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx,
    })
}

#[cfg(unix)]
fn open_unix_pty(size: TerminalSize) -> Result<(File, File)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut size = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let winp = std::ptr::addr_of_mut!(size);

    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            winp,
        )
    };
    if result != 0 {
        anyhow::bail!("failed to openpty: {:?}", std::io::Error::last_os_error());
    }

    set_cloexec(master)?;
    set_cloexec(slave)?;

    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

#[cfg(unix)]
fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn close_inherited_fds_except(preserved_fds: &[RawFd]) {
    if let Ok(dir) = std::fs::read_dir("/dev/fd") {
        let mut fds = Vec::new();
        for entry in dir {
            let num = entry
                .ok()
                .map(|entry| entry.file_name())
                .and_then(|name| name.into_string().ok())
                .and_then(|name| name.parse::<RawFd>().ok());
            if let Some(num) = num {
                if num <= 2 || preserved_fds.contains(&num) {
                    continue;
                }
                // Keep CLOEXEC descriptors open so std::process can still use
                // its internal exec-error pipe to report spawn failures.
                let flags = unsafe { libc::fcntl(num, libc::F_GETFD) };
                if flags == -1 || flags & libc::FD_CLOEXEC != 0 {
                    continue;
                }
                fds.push(num);
            }
        }
        for fd in fds {
            unsafe {
                libc::close(fd);
            }
        }
    }
}
