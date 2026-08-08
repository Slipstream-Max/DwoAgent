use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
#[cfg(not(windows))]
use uuid::Uuid;

use super::DEFAULT_PROFILE;
#[cfg(target_os = "macos")]
use super::home_dir;

pub(super) fn install(config_path: &Path) -> Result<()> {
    let root = config_path.parent().context("config path has no parent")?;
    let executable = install_executable(root)?;
    expose_executable(root.join("bin"))?;
    std::fs::create_dir_all(root.join("resource/prompts"))?;
    std::fs::create_dir_all(root.join("resource/skills"))?;
    std::fs::create_dir_all(root.join("runtime/sessions"))?;
    std::fs::create_dir_all(root.join("resource/mcp"))?;
    std::fs::create_dir_all(root.join("channels"))?;
    write_if_missing(config_path, DEFAULT_PROFILE)?;
    write_if_missing(
        &root.join("resource/prompts/System.md"),
        "You are a coding agent. Work carefully and report concrete results.\n",
    )?;
    write_if_missing(&root.join("resource/prompts/AGENTS.md"), "")?;
    write_if_missing(
        &root.join("resource/mcp/mcp.json"),
        "{\n  \"mcpServers\": {}\n}\n",
    )?;
    register_service(config_path, &executable)
}

fn install_executable(root: &Path) -> Result<PathBuf> {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin)?;
    let executable_name = if cfg!(windows) { "dwo.exe" } else { "dwo" };
    let destination = bin.join(executable_name);
    let source = std::env::current_exe()?;
    if destination.exists()
        && std::fs::canonicalize(&source)? == std::fs::canonicalize(&destination)?
    {
        return Ok(destination);
    }

    install_executable_file(&source, &destination, executable_name)?;
    Ok(destination)
}

#[cfg(windows)]
fn install_executable_file(source: &Path, destination: &Path, _name: &str) -> Result<()> {
    let contents = std::fs::read(source)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match dwo_agent_service::atomic_file::write_sync(destination, &contents) {
            Ok(()) => return Ok(()),
            Err(error) if std::time::Instant::now() < deadline => {
                tracing::debug!(
                    event = "install.executable_locked",
                    error = %format!("{error:#}"),
                    "wait for installed executable to become replaceable"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(windows))]
fn install_executable_file(source: &Path, destination: &Path, name: &str) -> Result<()> {
    let temporary = destination.with_file_name(format!(".{name}.{}.tmp", Uuid::new_v4()));
    std::fs::copy(source, &temporary)
        .with_context(|| format!("install executable at {}", destination.display()))?;
    let result = std::fs::rename(&temporary, destination)
        .with_context(|| format!("install executable at {}", destination.display()));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn expose_executable(bin: PathBuf) -> Result<()> {
    let status = ProcessCommand::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "$bin = $env:DWO_INSTALL_BIN; $path = [Environment]::GetEnvironmentVariable('Path', 'User'); $entries = @($path -split ';' | Where-Object { $_ }); if (-not ($entries | Where-Object { $_.TrimEnd('\\') -ieq $bin.TrimEnd('\\') })) { [Environment]::SetEnvironmentVariable('Path', (($entries + $bin) -join ';'), 'User') }",
        ])
        .env("DWO_INSTALL_BIN", &bin)
        .status()?;
    if !status.success() {
        bail!("failed to add {} to the user PATH", bin.display());
    }
    Ok(())
}

#[cfg(not(windows))]
fn expose_executable(_bin: PathBuf) -> Result<()> {
    Ok(())
}

fn write_if_missing(path: &Path, content: &str) -> Result<()> {
    if !path.exists() {
        std::fs::write(path, content)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn install_files_preserve_existing_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.yaml");
        std::fs::write(&path, "user configuration\n").unwrap();

        write_if_missing(&path, "replacement\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "user configuration\n"
        );
    }
}

#[cfg(windows)]
fn register_service(config_path: &Path, executable: &Path) -> Result<()> {
    let root = config_path.parent().context("config path has no parent")?;
    let launcher = root.join("bin/dwo-daemon.vbs");
    let command = format!("\"{}\" serve", executable.display());
    let script = format!(
        "Set shell = CreateObject(\"WScript.Shell\")\r\nexitCode = shell.Run(\"{}\", 0, True)\r\nWScript.Quit exitCode\r\n",
        command.replace('"', "\"\"")
    );
    std::fs::write(&launcher, script)?;
    let task = format!("wscript.exe \"{}\"", launcher.display());
    let status = ProcessCommand::new("schtasks.exe")
        .args(["/Create", "/SC", "ONLOGON", "/TN", "dwoagent", "/TR"])
        .arg(task)
        .args(["/F"])
        .status()?;
    if !status.success() {
        bail!("failed to register dwoagent startup task");
    }
    let settings = ProcessCommand::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "$task = Get-ScheduledTask -TaskName 'dwoagent'; $task.Settings.DisallowStartIfOnBatteries = $false; $task.Settings.StopIfGoingOnBatteries = $false; $task.Settings.ExecutionTimeLimit = 'PT0S'; Set-ScheduledTask -InputObject $task | Out-Null",
        ])
        .status()?;
    if !settings.success() {
        bail!("failed to configure dwoagent startup task");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn register_service(_config_path: &Path, executable: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let launch_agents = home_dir()?.join("Library/LaunchAgents");
    std::fs::create_dir_all(&launch_agents)?;
    let plist = launch_agents.join("com.dwoagent.host.plist");
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.dwoagent.host</string>
<key>ProgramArguments</key><array><string>{}</string><string>serve</string></array>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
</dict></plist>"#,
        executable.display()
    );
    std::fs::write(&plist, body)?;
    std::fs::set_permissions(&plist, std::fs::Permissions::from_mode(0o600))?;
    let _ = ProcessCommand::new("launchctl")
        .args(["bootstrap", &format!("gui/{}", unsafe { libc::geteuid() })])
        .arg(&plist)
        .status();
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn register_service(_config_path: &Path, _executable: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
pub(super) fn unregister_service(_config_path: &Path) -> Result<()> {
    let _ = ProcessCommand::new("schtasks.exe")
        .args(["/Delete", "/TN", "dwoagent", "/F"])
        .status()?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn unregister_service(_config_path: &Path) -> Result<()> {
    let plist = home_dir()?.join("Library/LaunchAgents/com.dwoagent.host.plist");
    let _ = ProcessCommand::new("launchctl")
        .args(["bootout", &format!("gui/{}", unsafe { libc::geteuid() })])
        .arg(&plist)
        .status();
    if plist.exists() {
        std::fs::remove_file(plist)?;
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn unregister_service(_config_path: &Path) -> Result<()> {
    Ok(())
}
