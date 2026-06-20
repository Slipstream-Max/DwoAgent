//! Local environment doctor.

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const MC_PORTER_NPM_PACKAGE: &str = "mcporter";
const RIPGREP_NPM_PACKAGE: &str = "ripgrep";
const NPM_COMMAND: &str = "npm";

#[derive(Debug, Clone)]
pub struct DoctorOptions {
    pub check: bool,
    pub resolve: bool,
    pub yes: bool,
}

pub fn run_doctor(options: DoctorOptions) -> Result<()> {
    let run_default_check = !options.check && !options.resolve;
    let mut checks = Vec::new();

    if options.resolve {
        resolve_environment(options.yes)?;
        checks.extend(collect_env_checks());
    } else if options.check || run_default_check {
        checks.extend(collect_env_checks());
    }

    print_checks(&checks);

    if checks
        .iter()
        .any(|check| matches!(check.status, CheckStatus::Missing))
    {
        bail!("doctor found issues");
    }

    println!("Doctor finished.");
    Ok(())
}

fn resolve_environment(yes: bool) -> Result<()> {
    ensure_npm_for_missing_tools(&["mcporter", "rg"])?;
    ensure_command("mcporter", MC_PORTER_NPM_PACKAGE, yes)?;
    ensure_command("rg", RIPGREP_NPM_PACKAGE, yes)?;
    Ok(())
}

fn ensure_npm_for_missing_tools(commands: &[&str]) -> Result<()> {
    let missing = commands
        .iter()
        .copied()
        .filter(|command| find_executable(command).is_none())
        .collect::<Vec<_>>();
    if missing.is_empty() || find_executable(NPM_COMMAND).is_some() {
        return Ok(());
    }

    bail!(
        "`npm` is required to install missing doctor tools: {}. Install Node.js/npm first, or install these tools manually and rerun doctor.",
        missing.join(", ")
    );
}

fn ensure_command(command: &str, npm_package: &str, yes: bool) -> Result<()> {
    if find_executable(command).is_some() {
        return Ok(());
    }

    let prompt = format!("Install `{command}` with `npm install -g {npm_package}`");
    if !confirm(yes, &prompt)? {
        return Ok(());
    }

    let npm = find_executable(NPM_COMMAND)
        .context("npm is required to install missing doctor tools; install Node.js/npm first")?;
    let status = Command::new(npm)
        .args(["install", "-g", npm_package])
        .status()
        .with_context(|| format!("run npm install -g {npm_package}"))?;
    if !status.success() {
        bail!("npm install -g {npm_package} failed");
    }
    Ok(())
}

#[derive(Debug)]
struct DoctorCheck {
    name: &'static str,
    status: CheckStatus,
    detail: String,
}

#[derive(Debug, Clone, Copy)]
enum CheckStatus {
    Ok,
    Missing,
    Skipped,
}

fn collect_env_checks() -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    let mcporter = command_check("mcporter");
    let rg = command_check("rg");
    let needs_npm = mcporter.is_missing() || rg.is_missing();
    checks.push(npm_check(needs_npm));
    checks.push(mcporter);
    checks.push(rg);
    checks
}

impl DoctorCheck {
    fn is_missing(&self) -> bool {
        matches!(self.status, CheckStatus::Missing)
    }
}

fn command_check(command: &'static str) -> DoctorCheck {
    match find_executable(command) {
        Some(path) => DoctorCheck {
            name: command,
            status: CheckStatus::Ok,
            detail: path.display().to_string(),
        },
        None => DoctorCheck {
            name: command,
            status: CheckStatus::Missing,
            detail: "not found in PATH".to_string(),
        },
    }
}

fn npm_check(required: bool) -> DoctorCheck {
    if !required {
        return DoctorCheck {
            name: "npm",
            status: CheckStatus::Skipped,
            detail: "not needed; doctor tools are already installed".to_string(),
        };
    }

    match find_executable(NPM_COMMAND) {
        Some(path) => DoctorCheck {
            name: "npm",
            status: CheckStatus::Ok,
            detail: format!("{} (needed for missing tools)", path.display()),
        },
        None => DoctorCheck {
            name: "npm",
            status: CheckStatus::Missing,
            detail: "required to install missing tools; install Node.js/npm first".to_string(),
        },
    }
}

fn print_checks(checks: &[DoctorCheck]) {
    for check in checks {
        let status = match check.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Missing => "missing",
            CheckStatus::Skipped => "skipped",
        };
        println!("[{status}] {}: {}", check.name, check.detail);
    }
}

fn confirm(yes: bool, prompt: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    print!("{prompt}? [y/N] ");
    io::stdout().flush().context("flush prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read prompt answer")?;
    let normalized = answer.trim().to_ascii_lowercase();
    Ok(normalized == "y" || normalized == "yes")
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(name);
    if candidate.components().count() > 1 && is_executable_file(&candidate) {
        return Some(candidate);
    }

    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        for candidate in executable_candidates(&dir, name) {
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let base = dir.join(name);
    if cfg!(windows) {
        let path = Path::new(name);
        if path.extension().is_some() {
            return vec![base];
        }
        let pathext =
            env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
        let mut candidates = vec![base.clone()];
        for ext in pathext.to_string_lossy().split(';') {
            candidates.push(dir.join(format!("{name}{ext}")));
        }
        candidates
    } else {
        vec![base]
    }
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_check_is_skipped_when_tools_do_not_need_install() {
        let check = npm_check(false);

        assert_eq!(check.name, "npm");
        assert!(matches!(check.status, CheckStatus::Skipped));
        assert!(check.detail.contains("not needed"));
    }
}
