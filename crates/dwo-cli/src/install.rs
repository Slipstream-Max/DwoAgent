use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result, bail};
#[cfg(not(windows))]
use uuid::Uuid;

use super::DEFAULT_PROFILE;
#[cfg(target_os = "macos")]
use super::home_dir;

pub(super) fn install(config_path: &Path) -> Result<()> {
    let root = config_path.parent().context("config path has no parent")?;
    let executable = install_executable(root)?;
    #[cfg(windows)]
    install_windows_self_tools(root)?;
    expose_executable(root.join("bin"))?;
    std::fs::create_dir_all(root.join("resource/prompts"))?;
    std::fs::create_dir_all(root.join("resource/skills"))?;
    std::fs::create_dir_all(root.join("runtime/sessions"))?;
    std::fs::create_dir_all(root.join("resource/mcp"))?;
    std::fs::create_dir_all(root.join("channels"))?;
    // Bundled runtime dependencies (niubash and Git) live here, separate
    // from dwo's own executable and resource directories.
    std::fs::create_dir_all(root.join("self-tools"))?;
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

#[cfg(windows)]
fn install_windows_self_tools(root: &Path) -> Result<()> {
    let tools = root.join("self-tools");
    std::fs::create_dir_all(&tools)?;
    let (niu_asset, niu_hash, git_asset, git_hash) = if cfg!(target_arch = "aarch64") {
        (
            "niubash-win-arm64.zip",
            "44736de66acdd4daaab21c64a3f4f09d7ed7d69a780acea7d8364e5a0a307eb8",
            "MinGit-2.55.0.5-arm64.zip",
            "05843f9d6e60306c3ab886799e2c67200caab921571f10512df3493049179ddb",
        )
    } else {
        (
            "niubash-win-x64.zip",
            "d08b525f17251792bba0e6ad82b3829084cf158774072573029dd89344992d71",
            "MinGit-2.55.0.5-64-bit.zip",
            "56d7b226b7693196cfc71fef26568f536c4a021ab6c37ff2db4287bed908e96e",
        )
    };
    let script = r#"
$ErrorActionPreference = 'Stop'
$tools = $env:DWO_SELF_TOOLS
$niuDir = Join-Path $tools 'niubash'
$gitDir = Join-Path $tools 'git'
$niuZip = Join-Path $env:TEMP 'dwo-niubash.zip'
$gitZip = Join-Path $env:TEMP 'dwo-mingit.zip'

function Install-Zip($url, $hash, $zip, $destination, $required) {
  $existing = Get-ChildItem -LiteralPath $destination -Recurse -Filter $required -File -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($null -ne $existing) {
    if ($existing.DirectoryName -ne $destination) {
      Get-ChildItem -LiteralPath $existing.DirectoryName -Force | Move-Item -Destination $destination -Force
      Remove-Item -LiteralPath $existing.DirectoryName -Recurse -Force
    }
    return
  }
  New-Item -ItemType Directory -Force -Path $destination | Out-Null
  & curl.exe --fail --location --retry 5 --retry-all-errors --retry-delay 2 --connect-timeout 20 --max-time 300 --silent --show-error $url --output $zip
  if ($LASTEXITCODE -ne 0) { throw "download failed for $url" }
  $sha = [System.Security.Cryptography.SHA256]::Create()
  try { $actual = ([BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($zip)))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
  if ($actual -ne $hash) {
    Remove-Item -Force -LiteralPath $zip
    throw "checksum mismatch for $url"
  }
  Expand-Archive -LiteralPath $zip -DestinationPath $destination -Force
  Remove-Item -Force -LiteralPath $zip
  # GitHub archives contain one versioned top-level directory. Flatten it so
  # runtime PATH entries remain stable across upgrades.
  $top = Get-ChildItem -LiteralPath $destination -Directory | Select-Object -First 1
  if ($null -ne $top -and $null -eq (Get-ChildItem -LiteralPath $destination -Filter $required -File -ErrorAction SilentlyContinue)) {
    Get-ChildItem -LiteralPath $top.FullName -Force | Move-Item -Destination $destination -Force
    Remove-Item -LiteralPath $top.FullName -Recurse -Force
  }
  $installed = Get-ChildItem -LiteralPath $destination -Recurse -Filter $required -File | Select-Object -First 1
  if ($null -eq $installed) { throw "archive did not contain $required" }
}

# Reuse applications already installed on the user's PATH. Bundled copies
# are only needed when the host machine has no usable dependency.
if ($null -eq (Get-Command niu.exe -CommandType Application -ErrorAction SilentlyContinue)) {
  Install-Zip $env:DWO_NIU_URL $env:DWO_NIU_HASH $niuZip $niuDir 'niu.exe'
}
if ($null -eq (Get-Command git.exe -CommandType Application -ErrorAction SilentlyContinue)) {
  Install-Zip $env:DWO_GIT_URL $env:DWO_GIT_HASH $gitZip $gitDir 'git.exe'
}
"#;
    let status = ProcessCommand::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .env("DWO_SELF_TOOLS", &tools)
        .env(
            "DWO_NIU_URL",
            format!("https://github.com/unixwin/niubash/releases/download/v1.1.0/{niu_asset}"),
        )
        .env("DWO_NIU_HASH", niu_hash)
        .env(
            "DWO_GIT_URL",
            format!("https://github.com/git-for-windows/git/releases/download/v2.55.0.windows.5/{git_asset}"),
        )
        .env("DWO_GIT_HASH", git_hash)
        .status()
        .context("download Windows self-tools")?;
    if !status.success() {
        bail!("failed to install niubash and Git into {}", tools.display());
    }
    Ok(())
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
    let exists = ProcessCommand::new("schtasks.exe")
        .args(["/Query", "/TN", "dwoagent"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success();
    if exists {
        return Ok(());
    }

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
