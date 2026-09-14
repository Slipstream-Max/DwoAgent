//! Locks profile state files while the host runs: on Windows a guarded file is
//! held open for read with `FILE_SHARE_READ`, so other handles may only read.
//! POSIX has no equivalent primitive and every call here is a no-op there.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    DenyWrite,
    Unsupported,
}

pub fn capability() -> Capability {
    if platform::SUPPORTED {
        Capability::DenyWrite
    } else {
        Capability::Unsupported
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Directory(&'static str),
    File(&'static str),
    Extension(&'static str),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    pub guarded: usize,
    pub failed: usize,
}

pub struct Protection {
    roots: Vec<PathBuf>,
    report: Report,
}

impl Protection {
    /// Best effort: a file that cannot be opened never prevents startup.
    pub fn install(profile_root: &Path, roots: &[&str], skips: &[Skip]) -> Protection {
        let roots: Vec<PathBuf> = roots
            .iter()
            .map(|root| normalize(&profile_root.join(root)))
            .collect();
        {
            let mut state = lock_state();
            for root in &roots {
                if !state.roots.contains(root) {
                    state.roots.push(root.clone());
                }
            }
            for skip in skips {
                if !state.skips.contains(skip) {
                    state.skips.push(*skip);
                }
            }
        }
        let mut report = Report::default();
        for root in &roots {
            guard_tree(root, &mut report);
        }
        Protection { roots, report }
    }

    pub fn report(&self) -> Report {
        self.report
    }

    pub fn release(&self) {
        drop_tree(&self.roots);
    }
}

impl Drop for Protection {
    fn drop(&mut self) {
        self.release();
    }
}

#[must_use = "the file is only writable while the permit lives"]
pub struct Permit {
    path: PathBuf,
    rearm: bool,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if self.rearm {
            protect_file(&self.path);
        }
    }
}

pub fn permit_file(path: &Path) -> Permit {
    let path = normalize(path);
    if platform::SUPPORTED {
        let handle = lock_state().handles.remove(&path);
        drop(handle);
    }
    Permit { path, rearm: true }
}

/// Drops guards under a subtree without re-arming them, for deletions.
pub fn release_tree(path: &Path) {
    let path = normalize(path);
    lock_state()
        .handles
        .retain(|key, _| !key.starts_with(&path));
}

fn protect_file(path: &Path) {
    if !platform::SUPPORTED {
        return;
    }
    let path = normalize(path);
    let mut state = lock_state();
    if state.handles.contains_key(&path) || !is_protected_path(&state, &path) {
        return;
    }
    if !path.is_file() {
        return;
    }
    if let Ok(handle) = platform::guard(&path) {
        state.handles.insert(path, handle);
    }
}

pub fn is_protected(path: &Path) -> bool {
    let path = normalize(path);
    lock_state().handles.contains_key(&path)
}

#[derive(Default)]
struct State {
    roots: Vec<PathBuf>,
    skips: Vec<Skip>,
    handles: HashMap<PathBuf, platform::Handle>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn lock_state() -> MutexGuard<'static, State> {
    state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn guard_tree(directory: &Path, report: &mut Report) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = normalize(&entry.path());
        if is_excluded(&path) {
            continue;
        }
        if file_type.is_dir() {
            guard_tree(&path, report);
        } else if file_type.is_file() {
            if guard_existing(&path).is_ok() {
                report.guarded += 1;
            } else {
                report.failed += 1;
            }
        }
    }
}

fn guard_existing(path: &Path) -> std::io::Result<()> {
    let mut state = lock_state();
    if state.handles.contains_key(path) {
        return Ok(());
    }
    let handle = platform::guard(path)?;
    state.handles.insert(path.to_path_buf(), handle);
    Ok(())
}

fn drop_tree(roots: &[PathBuf]) {
    lock_state()
        .handles
        .retain(|path, _| !roots.iter().any(|root| path.starts_with(root)));
}

fn is_excluded(path: &Path) -> bool {
    let state = lock_state();
    !is_protected_path(&state, path)
}

fn is_protected_path(state: &State, path: &Path) -> bool {
    let Some(root) = state.roots.iter().find(|root| path.starts_with(root)) else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if state
        .skips
        .iter()
        .any(|skip| matches!(skip, Skip::File(pattern) if *pattern == name))
    {
        return false;
    }
    if state.skips.iter().any(|skip| {
        matches!(skip, Skip::Extension(extension)
            if path.extension().and_then(|value| value.to_str()) == Some(*extension))
    }) {
        return false;
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if relative.components().any(|component| match component {
        Component::Normal(name) => name.to_str().is_some_and(|name| {
            state
                .skips
                .iter()
                .any(|skip| matches!(skip, Skip::Directory(pattern) if *pattern == name))
        }),
        _ => false,
    }) {
        return false;
    }
    true
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        normalized.push(component.as_os_str());
    }
    normalized
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    pub type Handle = File;
    pub const SUPPORTED: bool = true;

    /// Denies write, delete and rename to every other handle.
    pub fn guard(path: &Path) -> io::Result<Handle> {
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::io;
    use std::path::Path;

    pub type Handle = ();
    pub const SUPPORTED: bool = false;

    pub fn guard(_path: &Path) -> io::Result<Handle> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(profile: &Path) -> Protection {
        Protection::install(
            profile,
            &["runtime"],
            &[
                Skip::Directory("workspaces"),
                Skip::Directory("workspace"),
                Skip::File("AGENTS.md"),
                Skip::File("overview.md"),
                Skip::Extension("log"),
                Skip::Extension("tmp"),
            ],
        )
    }

    fn runtime_root(profile: &Path) -> PathBuf {
        let root = profile.join("runtime/sessions");
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn guards_deny_writes_until_the_permit_drops() {
        if capability() != Capability::DenyWrite {
            return;
        }
        let profile = tempfile::tempdir().unwrap();
        let runtime = runtime_root(profile.path());
        let state = runtime.join("session.json");
        std::fs::write(&state, "{}").unwrap();

        let protection = install(profile.path());
        assert_eq!(protection.report().guarded, 1);
        assert!(is_protected(&state));
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "{}");
        assert!(std::fs::write(&state, "blocked").is_err());
        assert!(std::fs::remove_file(&state).is_err());

        {
            let _permit = permit_file(&state);
            std::fs::write(&state, "{\"turn\":1}").unwrap();
        }

        assert!(std::fs::write(&state, "blocked").is_err());
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "{\"turn\":1}");

        protection.release();
        std::fs::write(&state, "after release").unwrap();
        assert!(!is_protected(&state));
    }

    #[test]
    fn guards_deny_replacing_a_file_renamed_over_it() {
        if capability() != Capability::DenyWrite {
            return;
        }
        let profile = tempfile::tempdir().unwrap();
        let runtime = runtime_root(profile.path());
        let state = runtime.join("session.json");
        std::fs::write(&state, "{}").unwrap();
        let _protection = install(profile.path());

        let temporary = runtime.join("session.json.7f3a.tmp");
        std::fs::write(&temporary, "{}").unwrap();
        assert!(std::fs::rename(&temporary, &state).is_err());

        {
            let _permit = permit_file(&state);
            std::fs::rename(&temporary, &state).unwrap();
        }
        assert!(std::fs::rename(&state, runtime.join("elsewhere.json")).is_err());
    }

    #[test]
    fn skips_keep_workspaces_rules_and_logs_writable() {
        if capability() != Capability::DenyWrite {
            return;
        }
        let profile = tempfile::tempdir().unwrap();
        let runtime = profile.path().join("runtime");
        let workspace = runtime.join("workspaces/session-1");
        let project = runtime.join("projects/project-1");
        let topic = project.join("topics/topic-1");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&topic).unwrap();
        std::fs::create_dir_all(project.join("workspace")).unwrap();
        let writable = [
            workspace.join("scratch.md"),
            project.join("workspace/checkout.txt"),
            project.join("AGENTS.md"),
            topic.join("AGENTS.md"),
            topic.join("overview.md"),
            runtime.join("restart.log"),
        ];
        for file in &writable {
            std::fs::write(file, "before").unwrap();
        }
        let guarded = project.join("project.json");
        std::fs::write(&guarded, "{}").unwrap();

        let _protection = install(profile.path());
        assert!(is_protected(&guarded));
        for file in &writable {
            assert!(
                !is_protected(file),
                "{} should stay writable",
                file.display()
            );
            std::fs::write(file, "after").unwrap();
        }
        assert!(std::fs::write(&guarded, "{}").is_err());
    }

    #[test]
    fn temporary_files_left_by_a_crash_stay_writable() {
        if capability() != Capability::DenyWrite {
            return;
        }
        let profile = tempfile::tempdir().unwrap();
        let project = profile.path().join("runtime/projects/project-1");
        std::fs::create_dir_all(&project).unwrap();
        let stale = project.join("project.tmp");
        std::fs::write(&stale, "half written").unwrap();

        let _protection = install(profile.path());
        assert!(!is_protected(&stale));
        std::fs::write(&stale, "next attempt").unwrap();
        std::fs::remove_file(&stale).unwrap();
    }

    #[test]
    fn files_outside_the_policy_are_left_alone() {
        let profile = tempfile::tempdir().unwrap();
        let runtime = runtime_root(profile.path());
        let guarded = runtime.join("session.json");
        std::fs::write(&guarded, "{}").unwrap();
        let outside = profile.path().join("profile.yaml");
        std::fs::write(&outside, "policyMode: confirm").unwrap();

        let _protection = install(profile.path());
        assert!(!is_protected(&outside));
        std::fs::write(&outside, "policyMode: watch").unwrap();
    }

    #[test]
    fn release_tree_drops_guards_for_deletions() {
        if capability() != Capability::DenyWrite {
            return;
        }
        let profile = tempfile::tempdir().unwrap();
        let runtime = runtime_root(profile.path());
        let state = runtime.join("session.json");
        std::fs::write(&state, "{}").unwrap();
        let _protection = install(profile.path());
        assert!(std::fs::remove_dir_all(&runtime).is_err());

        release_tree(&runtime);
        assert!(!is_protected(&state));
        std::fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn unsupported_platforms_leave_every_file_writable() {
        if capability() == Capability::DenyWrite {
            return;
        }
        let profile = tempfile::tempdir().unwrap();
        let runtime = runtime_root(profile.path());
        let state = runtime.join("session.json");
        std::fs::write(&state, "{}").unwrap();

        let _protection = install(profile.path());
        assert!(!is_protected(&state));
        std::fs::write(&state, "{}").unwrap();
    }
}
