use std::path::PathBuf;

/// The command shell the terminal and the environment prompt agree on.
///
/// Detected once per call site from the same sources, so the `shell` field
/// reported to the model always matches the wrapper the terminal spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shell {
    /// niubash, a native Windows Bash implementation with Windows path
    /// semantics and direct execution of Windows binaries.
    NiuBash(PathBuf),
    /// POSIX shell on Unix platforms.
    Sh,
}

impl Shell {
    pub fn name(&self) -> &'static str {
        match self {
            Self::NiuBash(_) => "bash",
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
    Shell::NiuBash(find_niubash().unwrap_or_else(|| PathBuf::from("niu.exe")))
}

/// Locate niubash (`niu.exe`). PATH is checked
/// first so portable installs and package managers work, followed by the
/// conventional per-user and machine install locations.
#[cfg(windows)]
fn find_niubash() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        for directory in std::env::split_paths(&path) {
            candidates.push(directory.join("niu.exe"));
        }
    }
    for variable in ["ProgramFiles", "LocalAppData"] {
        if let Some(base) = std::env::var_os(variable) {
            let base = PathBuf::from(base);
            candidates.push(base.join("niubash").join("niu.exe"));
            candidates.push(base.join("Programs").join("niubash").join("niu.exe"));
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let tools = PathBuf::from(profile).join(".dwoagent").join("self-tools");
        candidates.push(tools.join("niu.exe"));
        candidates.push(tools.join("niubash").join("niu.exe"));
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
                Shell::NiuBash(_) => "bash",
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
    fn niubash_is_the_only_windows_shell() {
        if let Some(path) = find_niubash() {
            assert!(path.is_file());
            assert_eq!(Shell::detect(), Shell::NiuBash(path));
        }
    }
}
