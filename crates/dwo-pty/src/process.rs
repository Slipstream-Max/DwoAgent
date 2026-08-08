use core::fmt;
use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use anyhow::anyhow;
#[cfg(not(unix))]
use portable_pty::MasterPty;
use portable_pty::PtySize;
use portable_pty::SlavePty;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessSignal {
    Interrupt,
}

#[cfg(not(unix))]
pub(crate) fn unsupported_signal(signal: ProcessSignal) -> io::Error {
    match signal {
        ProcessSignal::Interrupt => io::Error::new(
            io::ErrorKind::Unsupported,
            "process interrupt is not supported by this process backend",
        ),
    }
}

#[cfg(unix)]
pub(crate) fn exit_code_from_status(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        return 128 + signal;
    }

    -1
}

pub(crate) trait ChildTerminator: Send + Sync {
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()>;

    fn kill(&mut self) -> io::Result<()>;
}

/// How the output reader task is stopped once the child has exited.
pub(crate) enum ReaderStop {
    /// The platform PTY teardown closes the reader without an explicit flag.
    #[cfg(not(unix))]
    None,
    /// Unix poll-based readers exit when this flag is set.
    #[cfg(unix)]
    PollFlag(Arc<AtomicBool>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(value: TerminalSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[cfg(unix)]
pub(crate) trait PtyHandleKeepAlive: Send {}

#[cfg(unix)]
impl<T: Send + ?Sized> PtyHandleKeepAlive for T {}

pub(crate) enum PtyMasterHandle {
    #[cfg(not(unix))]
    Resizable(Box<dyn MasterPty + Send>),
    #[cfg(unix)]
    Opaque {
        raw_fd: RawFd,
        _handle: Box<dyn PtyHandleKeepAlive>,
    },
}

pub struct PtyHandles {
    pub _slave: Option<Box<dyn SlavePty + Send>>,
    pub(crate) _master: PtyMasterHandle,
}

impl fmt::Debug for PtyHandles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyHandles").finish()
    }
}

/// Handle for driving an interactive PTY process.
pub struct ProcessHandle {
    writer_tx: StdMutex<Option<mpsc::Sender<Vec<u8>>>>,
    killer: StdMutex<Option<Box<dyn ChildTerminator>>>,
    reader_handle: StdMutex<Option<JoinHandle<()>>>,
    writer_handle: StdMutex<Option<JoinHandle<()>>>,
    wait_handle: StdMutex<Option<JoinHandle<()>>>,
    exit_status: Arc<AtomicBool>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    // PtyHandles must be preserved because the process will receive Control+C if the
    // slave is closed
    _pty_handles: StdMutex<Option<PtyHandles>>,
    reader_stop: ReaderStop,
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessHandle").finish()
    }
}

impl ProcessHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        writer_tx: mpsc::Sender<Vec<u8>>,
        killer: Box<dyn ChildTerminator>,
        reader_handle: JoinHandle<()>,
        writer_handle: JoinHandle<()>,
        wait_handle: JoinHandle<()>,
        exit_status: Arc<AtomicBool>,
        exit_code: Arc<StdMutex<Option<i32>>>,
        pty_handles: Option<PtyHandles>,
        reader_stop: ReaderStop,
    ) -> Self {
        Self {
            writer_tx: StdMutex::new(Some(writer_tx)),
            killer: StdMutex::new(Some(killer)),
            reader_handle: StdMutex::new(Some(reader_handle)),
            writer_handle: StdMutex::new(Some(writer_handle)),
            wait_handle: StdMutex::new(Some(wait_handle)),
            exit_status,
            exit_code,
            _pty_handles: StdMutex::new(pty_handles),
            reader_stop,
        }
    }

    /// Returns a channel sender for writing raw bytes to the child stdin.
    pub fn writer_sender(&self) -> mpsc::Sender<Vec<u8>> {
        if let Ok(writer_tx) = self.writer_tx.lock()
            && let Some(writer_tx) = writer_tx.as_ref()
        {
            return writer_tx.clone();
        }

        let (writer_tx, writer_rx) = mpsc::channel(1);
        drop(writer_rx);
        writer_tx
    }

    /// True if the child process has exited.
    pub fn has_exited(&self) -> bool {
        self.exit_status.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Returns the exit code if known.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code.lock().ok().and_then(|guard| *guard)
    }

    /// Resize the PTY in character cells.
    pub fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        {
            let handles = self
                ._pty_handles
                .lock()
                .map_err(|_| anyhow!("failed to lock PTY handles"))?;
            if let Some(handles) = handles.as_ref() {
                return match &handles._master {
                    #[cfg(not(unix))]
                    PtyMasterHandle::Resizable(master) => master.resize(size.into()),
                    #[cfg(unix)]
                    PtyMasterHandle::Opaque { raw_fd, .. } => resize_raw_pty(*raw_fd, size),
                };
            }
        }

        Err(anyhow!("process is not attached to a PTY"))
    }

    /// Close the child's stdin channel.
    pub fn close_stdin(&self) {
        if let Ok(mut writer_tx) = self.writer_tx.lock() {
            writer_tx.take();
        }
    }

    /// Attempts to kill the child while leaving the reader/writer tasks alive
    /// so callers can still drain output until EOF.
    pub fn request_terminate(&self) {
        if let Ok(mut killer_opt) = self.killer.lock()
            && let Some(mut killer) = killer_opt.take()
        {
            let _ = killer.kill();
        }
    }

    pub fn signal(&self, signal: ProcessSignal) -> io::Result<()> {
        let Ok(mut killer_opt) = self.killer.lock() else {
            return Ok(());
        };
        let Some(killer) = killer_opt.as_mut() else {
            return Ok(());
        };

        killer.signal(signal)
    }

    /// Attempts to kill the child and abort helper tasks.
    pub fn terminate(&self) {
        self.request_terminate();

        if let Ok(mut h) = self.reader_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut h) = self.writer_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut h) = self.wait_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
    }

    /// Stop the output reader task so the stdout channel closes.
    ///
    /// Call after the child has exited (or during teardown). On Windows the
    /// PTY master owns the ConPTY output pipe write end; dropping it fails
    /// the reader's pending read with `ERROR_BROKEN_PIPE`, so the blocking
    /// thread exits on its own. On Unix the poll-based reader observes the
    /// cancel flag within one poll interval.
    pub fn shutdown_readers(&self) {
        match &self.reader_stop {
            #[cfg(unix)]
            ReaderStop::PollFlag(flag) => {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            #[cfg(not(unix))]
            ReaderStop::None => {}
        }
        if let Ok(mut handles) = self._pty_handles.lock() {
            *handles = None;
        }
        if let Ok(mut handle) = self.reader_handle.lock()
            && let Some(handle) = handle.take()
        {
            handle.abort();
        }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn resize_raw_pty(raw_fd: RawFd, size: TerminalSize) -> anyhow::Result<()> {
    let mut winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { libc::ioctl(raw_fd, libc::TIOCSWINSZ, &mut winsize) };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Return value from a PTY spawn helper.
#[derive(Debug)]
pub struct SpawnedProcess {
    pub session: ProcessHandle,
    pub stdout_rx: mpsc::Receiver<Vec<u8>>,
    pub stderr_rx: mpsc::Receiver<Vec<u8>>,
    pub exit_rx: oneshot::Receiver<i32>,
}
