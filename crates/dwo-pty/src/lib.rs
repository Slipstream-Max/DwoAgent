mod process;
pub mod process_group;
pub mod pty;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod windows_input;

pub const DEFAULT_OUTPUT_BYTES_CAP: usize = 1024 * 1024;

/// Handle for interacting with a spawned PTY process.
pub use process::ProcessHandle;
/// Process signal supported by spawned-process handles.
pub use process::ProcessSignal;
/// Bundle of process handles plus split output and exit receivers returned by spawn helpers.
pub use process::SpawnedProcess;
/// Terminal size in character cells used for PTY spawn and resize operations.
pub use process::TerminalSize;
/// Report whether ConPTY is available on this platform (Windows only).
pub use pty::conpty_supported;
/// Spawn a process attached to a PTY for interactive use.
pub use pty::spawn_process as spawn_pty_process;
#[cfg(windows)]
pub use win::PsuedoCon;
#[cfg(windows)]
pub use win::conpty::RawConPty;
#[cfg(windows)]
pub use windows_input::WindowsTtyInputNormalizer;
