//! Process-group helpers shared by pipe/pty and shell command execution.
//!
//! Platform-specific implementations are intentionally kept together so the
//! PTY crate has the same lifecycle operations on every supported target.

use std::io;

use tokio::process::Child;

#[cfg(target_os = "linux")]
pub fn set_parent_death_signal(parent_pid: libc::pid_t) -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } == -1 {
        return Err(io::Error::last_os_error());
    }

    if unsafe { libc::getppid() } != parent_pid {
        unsafe {
            libc::raise(libc::SIGTERM);
        }
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_parent_death_signal(_parent_pid: i32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn detach_from_tty() -> io::Result<()> {
    let result = unsafe { libc::setsid() };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            return set_process_group();
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn detach_from_tty() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn set_process_group() -> io::Result<()> {
    let result = unsafe { libc::setpgid(0, 0) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
pub fn set_process_group() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn kill_process_group_by_pid(pid: u32) -> io::Result<()> {
    use std::io::ErrorKind;

    let pid = pid as libc::pid_t;
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid == -1 {
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::NotFound && err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
        return Ok(());
    }

    let result = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::NotFound && err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn kill_process_group_by_pid(_pid: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn signal_process_group_id(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<bool> {
    use std::io::ErrorKind;

    let result = unsafe { libc::killpg(pgid, signal) };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.kind() == ErrorKind::NotFound || err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(err);
    }

    Ok(true)
}

#[cfg(unix)]
pub fn terminate_process_group(process_group_id: u32) -> io::Result<bool> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGTERM)
}

#[cfg(not(unix))]
pub fn terminate_process_group(_process_group_id: u32) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
pub fn interrupt_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGINT).map(|_| ())
}

#[cfg(not(unix))]
pub fn interrupt_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn kill_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGKILL).map(|_| ())
}

#[cfg(not(unix))]
pub fn kill_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn kill_child_process_group(child: &mut Child) -> io::Result<()> {
    if let Some(pid) = child.id() {
        return kill_process_group_by_pid(pid);
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn kill_child_process_group(_child: &mut Child) -> io::Result<()> {
    Ok(())
}
