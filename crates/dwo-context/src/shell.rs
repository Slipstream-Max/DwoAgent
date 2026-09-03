use std::path::PathBuf;

/// The command shell the terminal and the environment prompt agree on.
///
/// Detected once per call site from the same sources, so the `shell` field
/// reported to the model always matches the wrapper the terminal spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shell {
    /// Git for Windows (MSYS2) bash resolved on this machine.
    GitBash(PathBuf),
    /// Windows cmd.exe fallback used when Git Bash is unavailable.
    Cmd,
    /// POSIX shell on Unix platforms.
    Sh,
}

impl Shell {
    pub fn name(&self) -> &'static str {
        match self {
            Self::GitBash(_) => "bash",
            Self::Cmd => "cmd",
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
    match find_git_bash() {
        Some(program) => Shell::GitBash(program),
        None => Shell::Cmd,
    }
}

/// Locate Git Bash, or `None` to fall back to cmd.exe.
///
/// Only Git-specific locations are considered. The WSL launcher at
/// `System32\bash.exe` must never win, so a generic `bash` on PATH is not an
/// acceptable candidate.
#[cfg(windows)]
fn find_git_bash() -> Option<PathBuf> {
    candidate_bash_programs()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

#[cfg(windows)]
fn candidate_bash_programs() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    // Git for Windows records its install root for machine- and user-scoped
    // setups; this also covers per-user installs whose PATH entry the daemon
    // process may not have inherited.
    for subkey in [
        "SOFTWARE\\GitForWindows",
        "SOFTWARE\\WOW6432Node\\GitForWindows",
    ] {
        for hive in [
            winreg::enums::HKEY_LOCAL_MACHINE,
            winreg::enums::HKEY_CURRENT_USER,
        ] {
            if let Some(install_root) =
                registry_install_root(winreg::RegKey::predef(hive), subkey)
            {
                push_bash_variants(&mut candidates, &install_root);
            }
        }
    }
    // A git.exe on PATH implies a Git install whose bash lives one level up
    // (git.exe sits in `Git\cmd`, bash in `Git\bin`).
    if let Ok(path) = std::env::var("PATH") {
        for directory in std::env::split_paths(&path) {
            if !directory.join("git.exe").is_file() {
                continue;
            }
            if let Some(install_root) = directory.parent() {
                push_bash_variants(&mut candidates, install_root);
            }
        }
    }
    for (variable, suffix) in [
        ("ProgramFiles", "Git"),
        ("ProgramFiles(x86)", "Git"),
        ("LocalAppData", r"Programs\Git"),
    ] {
        if let Some(base) = std::env::var_os(variable) {
            push_bash_variants(&mut candidates, &std::path::PathBuf::from(base).join(suffix));
        }
    }
    dedup_paths(candidates)
}

#[cfg(windows)]
fn push_bash_variants(candidates: &mut Vec<PathBuf>, install_root: &std::path::Path) {
    candidates.push(install_root.join("bin").join("bash.exe"));
    candidates.push(install_root.join("usr").join("bin").join("bash.exe"));
}

#[cfg(windows)]
fn registry_install_root(root: winreg::RegKey, subkey: &str) -> Option<PathBuf> {
    let key = root.open_subkey(subkey).ok()?;
    let install_path: String = key.get_value("InstallPath").ok()?;
    Some(PathBuf::from(install_path))
}

#[cfg(windows)]
fn dedup_paths(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate in candidates {
        let key = candidate.to_string_lossy().to_ascii_lowercase();
        if seen.insert(key) {
            unique.push(candidate);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected_shell_name_matches_the_variant() {
        let shell = Shell::detect();
        if cfg!(windows) {
            let expected = match &shell {
                Shell::GitBash(_) => "bash",
                Shell::Cmd => "cmd",
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
    fn git_bash_candidates_include_common_install_roots() {
        let candidates = candidate_bash_programs();
        let program_files = std::env::var_os("ProgramFiles")
            .map(|base| std::path::PathBuf::from(base).join("Git").join("bin").join("bash.exe"));
        if let Some(expected) = program_files {
            assert!(
                candidates.contains(&expected),
                "candidates missing {expected:?}: {candidates:?}"
            );
        }
    }
}
