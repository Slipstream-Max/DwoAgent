use std::path::PathBuf;

/// The command shell the terminal and the environment prompt agree on.
///
/// Detected once per call site from the same sources, so the `shell` field
/// reported to the model always matches the wrapper the terminal spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shell {
    /// PowerShell 7 (`pwsh.exe`) on Windows.
    Pwsh(PathBuf),
    /// POSIX shell on Unix platforms.
    Sh,
}

impl Shell {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pwsh(_) => "pwsh",
            Self::Sh => "sh",
        }
    }

    pub fn detect() -> Self {
        if cfg!(windows) {
            windows_shell()
        } else {
            Self::Sh
        }
    }
}

#[cfg(windows)]
fn windows_shell() -> Shell {
    Shell::Pwsh(find_pwsh().unwrap_or_else(|| PathBuf::from("pwsh.exe")))
}

/// Locate PowerShell 7 (`pwsh.exe`), preferring the copy bundled under
/// `self-tools` and then PATH, so portable installs and package managers
/// work, followed by the conventional machine-wide, per-user, and Microsoft
/// Store install locations.
#[cfg(windows)]
fn find_pwsh() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let tools = PathBuf::from(profile).join(".dwoagent").join("self-tools");
        candidates.push(tools.join("pwsh").join("pwsh.exe"));
    }
    if let Ok(path) = std::env::var("PATH") {
        for directory in std::env::split_paths(&path) {
            candidates.push(directory.join("pwsh.exe"));
        }
    }
    if let Some(base) = std::env::var_os("ProgramFiles") {
        let base = PathBuf::from(base);
        candidates.push(base.join("PowerShell").join("7").join("pwsh.exe"));
        candidates.push(base.join("PowerShell").join("7-preview").join("pwsh.exe"));
    }
    if let Some(base) = std::env::var_os("LocalAppData") {
        let base = PathBuf::from(base);
        candidates.push(
            base.join("Microsoft")
                .join("PowerShell")
                .join("7")
                .join("pwsh.exe"),
        );
        candidates.push(base.join("Microsoft").join("WindowsApps").join("pwsh.exe"));
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected_shell_name_matches_the_variant() {
        let shell = Shell::detect();
        if cfg!(windows) {
            let expected = match &shell {
                Shell::Pwsh(_) => "pwsh",
                Shell::Sh => unreachable!(),
            };
            assert_eq!(shell.name(), expected);
        } else {
            assert_eq!(shell, Shell::Sh);
            assert_eq!(shell.name(), "sh");
        }
    }

    #[cfg(windows)]
    #[test]
    fn pwsh_is_the_windows_shell_when_present() {
        if let Some(path) = find_pwsh() {
            assert!(path.is_file());
            assert_eq!(Shell::detect(), Shell::Pwsh(path));
        }
    }
}
