use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use dwo_mcp::SearchGroup;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::automation::{AutomationJobStatus, AutomationRunRecord};
use crate::channels::{
    self, ChannelKind, FeishuBindProgress, TelegramBindProgress, WeixinLoginProgress,
};
use crate::host;
use crate::local::{acp, ipc};

mod render;

#[derive(Parser)]
#[command(name = "dwo", version, about = "dwoagent host and control CLI")]
struct Cli {
    #[arg(long, global = true, alias = "configpath")]
    config_path: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Install {
        #[arg(long)]
        start: bool,
    },
    Uninstall {
        #[arg(long)]
        purge: bool,
    },
    Serve,
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    ProfileList,
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    Automation {
        #[command(subcommand)]
        command: AutomationCommand,
    },
    Acp,
}

#[derive(Subcommand)]
enum DaemonCommand {
    Start,
    Stop,
    Status,
}

#[derive(Subcommand)]
enum SessionCommand {
    List {
        #[arg(long)]
        all: bool,
    },
    Delete {
        id: String,
    },
    Prompt {
        message: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        policy: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        reasoning: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    Cancel {
        id: String,
    },
    Watch {
        id: String,
        #[arg(long)]
        cursor: Option<usize>,
        #[arg(long, default_value_t = 3)]
        limit: usize,
    },
    Approve {
        id: String,
        permission_id: String,
    },
    Deny {
        id: String,
        permission_id: String,
    },
}

#[derive(Subcommand)]
enum ChannelCommand {
    List,
    Weixin {
        #[command(subcommand)]
        command: ManagedChannelCommand,
    },
    Telegram {
        #[command(subcommand)]
        command: ManagedChannelCommand,
    },
    Feishu {
        #[command(subcommand)]
        command: ManagedChannelCommand,
    },
}

#[derive(Subcommand)]
enum ManagedChannelCommand {
    Status,
    Bind,
    Unbind,
    SendMessage { message: String },
    SendFile { path: PathBuf },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Search configured MCP servers and tools.
    Search {
        /// Case-insensitive terms matched against server and tool metadata.
        query: String,
    },
    /// Call one MCP tool using a server.tool selector.
    Call {
        selector: String,
        #[arg(long, default_value = "{}")]
        args: String,
    },
    /// Authorize a server, or remove its stored authorization.
    Auth {
        server: String,
        #[arg(long)]
        logout: bool,
    },
}

#[derive(Subcommand)]
enum AutomationCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Run {
        job: String,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config_path.unwrap_or(default_config_path()?);
    match cli.command {
        Command::Install { start } => {
            install(&config_path)?;
            println!("Installed profile at {}", config_path.display());
            if start {
                daemon_start(&config_path).await?;
            }
        }
        Command::Uninstall { purge } => {
            let _ = ipc::request(&config_path, "daemon.shutdown", json!({})).await;
            unregister_service(&config_path)?;
            if purge {
                let root = config_path.parent().context("config path has no parent")?;
                if root.exists() {
                    std::fs::remove_dir_all(root)?;
                }
            }
            println!(
                "Uninstalled dwoagent{}",
                if purge { " and removed profile" } else { "" }
            );
        }
        Command::Serve => {
            let host = host::Host::load(&config_path).await?;
            println!("dwoagent serving {}", ipc::endpoint(&config_path));
            ipc::serve(host, &config_path).await?;
        }
        Command::Daemon { command } => match command {
            DaemonCommand::Start => daemon_start(&config_path).await?,
            DaemonCommand::Stop => {
                ipc::request(&config_path, "daemon.shutdown", json!({})).await?;
                println!("Stopping dwoagent daemon");
            }
            DaemonCommand::Status => {
                let status = ipc::request(&config_path, "daemon.status", json!({})).await?;
                print_value(&status)?;
            }
        },
        Command::Session { command } => run_session(command, &config_path).await?,
        Command::ProfileList => {
            let value = ipc::request(&config_path, "profile.list", json!({})).await?;
            print_value(&value)?;
        }
        Command::Channel { command } => run_channel(command, &config_path).await?,
        Command::Mcp { command } => run_mcp(command, &config_path).await?,
        Command::Automation { command } => run_automation(command, &config_path).await?,
        Command::Acp => acp::run(config_path).await?,
    }
    Ok(())
}

async fn run_automation(command: AutomationCommand, config_path: &Path) -> Result<()> {
    match command {
        AutomationCommand::List { json } | AutomationCommand::Status { json } => {
            let value = ipc::request(config_path, "automation.list", json!({})).await?;
            if json {
                print_value(&value)?;
            } else {
                let jobs: Vec<AutomationJobStatus> = serde_json::from_value(value)?;
                if jobs.is_empty() {
                    println!("No automation jobs configured");
                }
                for status in jobs {
                    let next = status.next_run_at.as_deref().unwrap_or("disabled");
                    let active = if status.active_runs.is_empty() {
                        String::new()
                    } else {
                        format!(" active={}", status.active_runs.len())
                    };
                    println!("{}  next={}{}", status.job.name, next, active);
                }
            }
        }
        AutomationCommand::Run { job, json } => {
            let value = ipc::request(config_path, "automation.run", json!({"job": job})).await?;
            if json {
                print_value(&value)?;
            } else {
                let record: AutomationRunRecord = serde_json::from_value(value)?;
                println!(
                    "{}  {:?}  session={}",
                    record.job,
                    record.status,
                    record.session_id.as_deref().unwrap_or("-")
                );
                if let Some(error) = record.error {
                    println!("error: {error}");
                }
                if !record.response.is_empty() {
                    println!("\n{}", record.response);
                }
            }
        }
    }
    Ok(())
}

async fn run_mcp(command: McpCommand, config_path: &Path) -> Result<()> {
    match command {
        McpCommand::Search { query } => {
            let value = ipc::request(config_path, "mcp.search", json!({"query": query})).await?;
            let groups: Vec<SearchGroup> = serde_json::from_value(value)?;
            print!("{}", dwo_mcp::render_search(&groups));
        }
        McpCommand::Call { selector, args } => {
            let arguments: Value = serde_json::from_str(&args).context("parse --args JSON")?;
            let value = ipc::request(
                config_path,
                "mcp.call",
                json!({"selector": selector, "arguments": arguments}),
            )
            .await?;
            print_value(&value)?;
        }
        McpCommand::Auth { server, logout } => {
            let method = if logout {
                "mcp.auth.logout"
            } else {
                println!("Opening the authorization page for {server}...");
                "mcp.auth.login"
            };
            ipc::request(config_path, method, json!({"server": server})).await?;
            println!(
                "Authorization {}",
                if logout { "removed" } else { "updated" }
            );
        }
    }
    Ok(())
}

async fn run_session(command: SessionCommand, config_path: &Path) -> Result<()> {
    let endpoint_id = format!("cli-{}", Uuid::new_v4());
    match command {
        SessionCommand::List { all } => {
            let value = ipc::request(
                config_path,
                "session.list",
                json!({"all": all, "caller_session_id": current_session_id()}),
            )
            .await?;
            render::print_session_list(&value)?;
        }
        SessionCommand::Delete { id } => {
            ipc::request(config_path, "session.delete", json!({"session_id": id})).await?;
            println!("Deleted session");
        }
        SessionCommand::Prompt {
            message,
            title,
            cwd,
            policy,
            model,
            reasoning,
            to,
        } => {
            let policy = policy
                .map(|value| dwo_tools::SessionMode::parse(&value).map_err(anyhow::Error::msg))
                .transpose()?;
            let value = ipc::request(
                config_path,
                "session.prompt",
                json!({
                    "session_id": to,
                    "caller_session_id": current_session_id(),
                    "endpoint_id": endpoint_id,
                    "message": message,
                    "title": title,
                    "cwd": cwd,
                    "policy": policy,
                    "model": model,
                    "reasoning": reasoning,
                }),
            )
            .await?;
            print_value(&value)?;
        }
        SessionCommand::Cancel { id } => {
            ipc::request(config_path, "session.cancel", json!({"session_id": id})).await?;
            println!("Cancellation requested");
        }
        SessionCommand::Watch { id, cursor, limit } => {
            let value = ipc::request(
                config_path,
                "session.read",
                json!({"session_id": id, "cursor": cursor, "limit": limit}),
            )
            .await?;
            print_value(&value)?;
        }
        SessionCommand::Approve { id, permission_id } => {
            permission(config_path, id, endpoint_id, permission_id, true, None).await?;
        }
        SessionCommand::Deny { id, permission_id } => {
            permission(config_path, id, endpoint_id, permission_id, false, None).await?;
        }
    }
    Ok(())
}

fn current_session_id() -> Option<String> {
    std::env::var("DWO_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

async fn permission(
    config_path: &Path,
    session_id: String,
    endpoint_id: String,
    request_id: String,
    allowed: bool,
    reason: Option<String>,
) -> Result<()> {
    ipc::request(
        config_path,
        "session.permission",
        json!({
            "session_id": session_id,
            "endpoint_id": endpoint_id,
            "request_id": request_id,
            "allowed": allowed,
            "reason": reason,
        }),
    )
    .await?;
    println!("Permission resolved");
    Ok(())
}

async fn run_channel(command: ChannelCommand, config_path: &Path) -> Result<()> {
    match command {
        ChannelCommand::List => {
            let value = ipc::request(config_path, "channel.list", json!({})).await?;
            print_value(&value)?;
        }
        ChannelCommand::Weixin { command } => {
            run_managed_channel(ChannelKind::Weixin, command, config_path).await?
        }
        ChannelCommand::Telegram { command } => {
            run_managed_channel(ChannelKind::Telegram, command, config_path).await?
        }
        ChannelCommand::Feishu { command } => {
            run_managed_channel(ChannelKind::Feishu, command, config_path).await?
        }
    }
    Ok(())
}

async fn run_managed_channel(
    channel: ChannelKind,
    command: ManagedChannelCommand,
    config_path: &Path,
) -> Result<()> {
    let method = |action| format!("channel.{}.{action}", channel.as_str());
    match command {
        ManagedChannelCommand::Status => {
            let value = ipc::request(config_path, &method("status"), json!({})).await?;
            print_value(&value)?;
        }
        ManagedChannelCommand::Unbind => {
            let value = ipc::request(config_path, &method("remove"), json!({})).await?;
            print_value(&value)?;
        }
        ManagedChannelCommand::SendMessage { message } => {
            let value = ipc::request(
                config_path,
                &method("send_message"),
                json!({"text": message}),
            )
            .await?;
            print_value(&value)?;
        }
        ManagedChannelCommand::SendFile { path } => {
            let value =
                ipc::request(config_path, &method("send_file"), json!({"path": path})).await?;
            print_value(&value)?;
        }
        ManagedChannelCommand::Bind => match channel {
            ChannelKind::Weixin => bind_weixin(config_path).await?,
            ChannelKind::Telegram => bind_telegram(config_path).await?,
            ChannelKind::Feishu => bind_feishu(config_path).await?,
        },
    }
    Ok(())
}

async fn bind_weixin(config_path: &Path) -> Result<()> {
    let start = ipc::request(config_path, "channel.weixin.begin", json!({})).await?;
    let binding_id = start["binding_id"]
        .as_str()
        .context("daemon omitted binding_id")?;
    let qrcode = start["qrcode"].as_str().context("daemon omitted qrcode")?;
    println!("Scan this QR code with Weixin:\n");
    if let Err(error) = qr2term::print_qr(qrcode) {
        eprintln!("Could not render terminal QR: {error}");
        println!("{qrcode}");
    }
    let mut verify_code: Option<String> = None;
    loop {
        channels::wait_before_poll().await;
        let progress = ipc::request(
            config_path,
            "channel.weixin.poll",
            json!({"binding_id": binding_id, "verify_code": verify_code.take()}),
        )
        .await?;
        let progress: WeixinLoginProgress = serde_json::from_value(progress)?;
        match progress {
            WeixinLoginProgress::Waiting => {}
            WeixinLoginProgress::Scanned => println!("Scanned; confirm on your phone"),
            WeixinLoginProgress::Confirmed { channel } => {
                println!("Channel {} connected", channel.name);
                break;
            }
            WeixinLoginProgress::NeedVerifyCode => {
                println!("Enter the verification code shown on your phone:");
                let mut code = String::new();
                std::io::stdin().read_line(&mut code)?;
                verify_code = Some(code.trim().to_string());
            }
            WeixinLoginProgress::Expired => bail!("QR code expired"),
            WeixinLoginProgress::Failed { message } => bail!(message),
        }
    }
    Ok(())
}

async fn daemon_start(config_path: &Path) -> Result<()> {
    if ipc::request(config_path, "daemon.status", json!({}))
        .await
        .is_ok()
    {
        println!("dwoagent daemon is already running");
        return Ok(());
    }
    if !start_registered_service()? {
        let executable = std::env::current_exe()?;
        let mut command = ProcessCommand::new(executable);
        command
            .arg("--config-path")
            .arg(config_path)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000 | 0x0000_0008 | 0x0000_0200);
        }
        command.spawn()?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(75);
    while tokio::time::Instant::now() < deadline {
        if ipc::request(config_path, "daemon.status", json!({}))
            .await
            .is_ok()
        {
            println!("dwoagent daemon started");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("daemon process started but did not become healthy")
}

#[cfg(windows)]
fn start_registered_service() -> Result<bool> {
    let exists = ProcessCommand::new("schtasks.exe")
        .args(["/Query", "/TN", "dwoagent"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success();
    if !exists {
        return Ok(false);
    }
    Ok(ProcessCommand::new("schtasks.exe")
        .args(["/Run", "/TN", "dwoagent"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

#[cfg(target_os = "macos")]
fn start_registered_service() -> Result<bool> {
    Ok(ProcessCommand::new("launchctl")
        .args([
            "kickstart",
            "-k",
            &format!("gui/{}/com.dwoagent.host", unsafe { libc::geteuid() }),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn start_registered_service() -> Result<bool> {
    Ok(false)
}

fn install(config_path: &Path) -> Result<()> {
    let root = config_path.parent().context("config path has no parent")?;
    let executable = install_executable(root)?;
    expose_executable(root.join("bin"))?;
    std::fs::create_dir_all(root.join("resource/prompts"))?;
    std::fs::create_dir_all(root.join("resource/skills"))?;
    std::fs::create_dir_all(root.join("runtime/sessions"))?;
    std::fs::create_dir_all(root.join("runtime/mcp"))?;
    std::fs::create_dir_all(root.join("runtime/logs"))?;
    std::fs::create_dir_all(root.join("channels"))?;
    write_if_missing(config_path, DEFAULT_PROFILE)?;
    write_if_missing(
        &root.join("resource/prompts/System.md"),
        "You are a coding agent. Work carefully and report concrete results.\n",
    )?;
    write_if_missing(&root.join("resource/prompts/AGENTS.md"), "")?;
    write_if_missing(
        &root.join("resource/mcp.json"),
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

    let temporary = bin.join(format!(".{executable_name}.{}.tmp", Uuid::new_v4()));
    std::fs::copy(&source, &temporary)
        .with_context(|| format!("install executable at {}", destination.display()))?;
    if destination.exists() {
        std::fs::remove_file(&destination)
            .with_context(|| format!("replace executable at {}", destination.display()))?;
    }
    if let Err(error) = std::fs::rename(&temporary, &destination) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("install executable at {}", destination.display()));
    }
    Ok(destination)
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

#[cfg(windows)]
fn register_service(config_path: &Path, executable: &Path) -> Result<()> {
    let root = config_path.parent().context("config path has no parent")?;
    let launcher = root.join("runtime/dwo-daemon.vbs");
    let command = format!(
        "\"{}\" --config-path \"{}\" serve",
        executable.display(),
        config_path.display()
    );
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
fn register_service(config_path: &Path, executable: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = home_dir()?;
    let launch_agents = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&launch_agents)?;
    let plist = launch_agents.join("com.dwoagent.host.plist");
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.dwoagent.host</string>
<key>ProgramArguments</key><array><string>{}</string><string>--config-path</string><string>{}</string><string>serve</string></array>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
</dict></plist>"#,
        executable.display(),
        config_path.display()
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
fn unregister_service(_config_path: &Path) -> Result<()> {
    let _ = ProcessCommand::new("schtasks.exe")
        .args(["/Delete", "/TN", "dwoagent", "/F"])
        .status()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn unregister_service(_config_path: &Path) -> Result<()> {
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
fn unregister_service(_config_path: &Path) -> Result<()> {
    Ok(())
}

fn default_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".dwoagent/profile.yaml"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .context("cannot determine user home directory")
}

fn print_value(value: &Value) -> Result<()> {
    render::print_value(value)
}

async fn bind_telegram(config_path: &Path) -> Result<()> {
    let start = ipc::request(config_path, "channel.telegram.begin", json!({})).await?;
    let binding_id = start["binding_id"]
        .as_str()
        .context("daemon omitted binding_id")?;
    let code = start["code"].as_str().context("daemon omitted bind code")?;
    let bot_username = start["bot_username"]
        .as_str()
        .context("daemon omitted bot_username")?;
    println!("Open @{bot_username} in Telegram and send this private message:\n");
    println!("/bind {code}\n");
    println!("Waiting for Telegram binding confirmation...");
    loop {
        channels::wait_before_poll().await;
        let progress = ipc::request(
            config_path,
            "channel.telegram.poll",
            json!({"binding_id": binding_id}),
        )
        .await?;
        match serde_json::from_value::<TelegramBindProgress>(progress)? {
            TelegramBindProgress::Waiting => {}
            TelegramBindProgress::Confirmed { channel } => {
                println!("Channel {} connected", channel.name);
                break;
            }
            TelegramBindProgress::Expired => bail!("Telegram binding code expired"),
        }
    }
    Ok(())
}

async fn bind_feishu(config_path: &Path) -> Result<()> {
    let start = ipc::request(config_path, "channel.feishu.begin", json!({})).await?;
    let binding_id = start["binding_id"]
        .as_str()
        .context("daemon omitted binding_id")?;
    let code = start["code"].as_str().context("daemon omitted bind code")?;
    let platform = start["platform"]
        .as_str()
        .context("daemon omitted Feishu platform")?;
    let product = if platform == "lark" { "Lark" } else { "Feishu" };
    println!("Open the application bot in {product} and send this private message:\n");
    println!("/bind {code}\n");
    println!("Waiting for {product} binding confirmation...");
    loop {
        channels::wait_before_poll().await;
        let progress = ipc::request(
            config_path,
            "channel.feishu.poll",
            json!({"binding_id": binding_id}),
        )
        .await?;
        match serde_json::from_value::<FeishuBindProgress>(progress)? {
            FeishuBindProgress::Waiting => {}
            FeishuBindProgress::Confirmed { channel } => {
                println!("Channel {} connected", channel.name);
                break;
            }
            FeishuBindProgress::Expired => bail!("Feishu binding code expired"),
            FeishuBindProgress::Failed { message } => bail!(message),
        }
    }
    Ok(())
}

const DEFAULT_PROFILE: &str = r#"name: coder
description: coding agent
policyMode: confirm
channels:
  weixin:
    enabled: true
    replayTurns: 5
    markdownFilter: true
  telegram:
    enabled: false
    replayTurns: 5
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true
  feishu:
    enabled: false
    replayTurns: 5
    appIdEnv: FEISHU_APP_ID
    appSecretEnv: FEISHU_APP_SECRET
    platform: feishu
    mediaInput: true
automation:
  enabled: false
  jobs: []
model:
  defaultModelId: deepseek-v4-pro
  providers:
    deepseek:
      type: deepseek
      apiKeyEnv: DEEPSEEK_API_KEY
  models:
    - modelName: deepseek-v4-pro
      provider: deepseek
      modelId: deepseek-v4-pro
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_weixin_commands() {
        let status = Cli::try_parse_from(["dwo", "channel", "weixin", "status"]).unwrap();
        assert!(matches!(
            status.command,
            Command::Channel {
                command: ChannelCommand::Weixin {
                    command: ManagedChannelCommand::Status
                }
            }
        ));

        let send =
            Cli::try_parse_from(["dwo", "channel", "weixin", "send-file", "report.pdf"]).unwrap();
        assert!(matches!(
            send.command,
            Command::Channel {
                command: ChannelCommand::Weixin {
                    command: ManagedChannelCommand::SendFile { ref path }
                }
            } if path == &PathBuf::from("report.pdf")
        ));
    }

    #[test]
    fn parses_telegram_commands() {
        let status = Cli::try_parse_from(["dwo", "channel", "telegram", "status"]).unwrap();
        assert!(matches!(
            status.command,
            Command::Channel {
                command: ChannelCommand::Telegram {
                    command: ManagedChannelCommand::Status
                }
            }
        ));

        let send =
            Cli::try_parse_from(["dwo", "channel", "telegram", "send-file", "clip.mp4"]).unwrap();
        assert!(matches!(
            send.command,
            Command::Channel {
                command: ChannelCommand::Telegram {
                    command: ManagedChannelCommand::SendFile { ref path }
                }
            } if path == &PathBuf::from("clip.mp4")
        ));
    }

    #[test]
    fn parses_feishu_commands() {
        let status = Cli::try_parse_from(["dwo", "channel", "feishu", "status"]).unwrap();
        assert!(matches!(
            status.command,
            Command::Channel {
                command: ChannelCommand::Feishu {
                    command: ManagedChannelCommand::Status
                }
            }
        ));

        let send =
            Cli::try_parse_from(["dwo", "channel", "feishu", "send-file", "report.pdf"]).unwrap();
        assert!(matches!(
            send.command,
            Command::Channel {
                command: ChannelCommand::Feishu {
                    command: ManagedChannelCommand::SendFile { ref path }
                }
            } if path == &PathBuf::from("report.pdf")
        ));
    }

    #[test]
    fn parses_minimal_mcp_commands() {
        let search = Cli::try_parse_from(["dwo", "mcp", "search", "install"]).unwrap();
        assert!(matches!(
            search.command,
            Command::Mcp {
                command: McpCommand::Search { ref query }
            } if query == "install"
        ));

        let auth = Cli::try_parse_from(["dwo", "mcp", "auth", "github", "--logout"]).unwrap();
        assert!(matches!(
            auth.command,
            Command::Mcp {
                command: McpCommand::Auth {
                    ref server,
                    logout: true,
                }
            } if server == "github"
        ));

        assert!(Cli::try_parse_from(["dwo", "mcp", "list"]).is_err());
        assert!(Cli::try_parse_from(["dwo", "mcp", "show", "github"]).is_err());
    }

    #[test]
    fn parses_automation_commands() {
        let run = Cli::try_parse_from(["dwo", "automation", "run", "daily-report"]).unwrap();
        assert!(matches!(
            run.command,
            Command::Automation {
                command: AutomationCommand::Run { ref job, json: false }
            } if job == "daily-report"
        ));
    }

    #[test]
    fn parses_subsession_commands() {
        let prompt = Cli::try_parse_from([
            "dwo",
            "session",
            "prompt",
            "inspect the failure",
            "--title",
            "inspector",
            "--policy",
            "watch",
            "--model",
            "fast",
            "--reasoning",
            "high",
        ])
        .unwrap();
        assert!(matches!(
            prompt.command,
            Command::Session {
                command: SessionCommand::Prompt {
                    ref message,
                    ref title,
                    ref policy,
                    ref model,
                    ref reasoning,
                    to: None,
                    ..
                }
            } if message == "inspect the failure"
                && title.as_deref() == Some("inspector")
                && policy.as_deref() == Some("watch")
                && model.as_deref() == Some("fast")
                && reasoning.as_deref() == Some("high")
        ));

        let watch = Cli::try_parse_from([
            "dwo",
            "session",
            "watch",
            "session-child",
            "--cursor",
            "12",
            "--limit",
            "5",
        ])
        .unwrap();
        assert!(matches!(
            watch.command,
            Command::Session {
                command: SessionCommand::Watch {
                    ref id,
                    cursor: Some(12),
                    limit: 5,
                }
            } if id == "session-child"
        ));

        assert!(Cli::try_parse_from(["dwo", "session", "new"]).is_err());
        assert!(Cli::try_parse_from(["dwo", "session", "model", "id", "model"]).is_err());
        assert!(Cli::try_parse_from(["dwo", "profile-list"]).is_ok());
        assert!(Cli::try_parse_from(["dwo", "profilelist"]).is_err());
    }
}
