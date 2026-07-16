use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use dwo_mcp::{Catalog, CatalogServer, CatalogTool, SearchGroup, ShowResult};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::automation::{AutomationJobStatus, AutomationRunRecord};
use crate::channels::{self, WeixinLoginProgress};
use crate::host;
use crate::local::{acp, ipc};

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
    List,
    New {
        name: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    Delete {
        id: String,
    },
    Prompt {
        id: String,
        message: String,
    },
    Cancel {
        id: String,
    },
    Watch {
        id: String,
    },
    Model {
        id: String,
        model: String,
    },
    Reasoning {
        id: String,
        reasoning: Option<String>,
    },
    Approve {
        id: String,
        permission_id: String,
    },
    Deny {
        id: String,
        permission_id: String,
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum ChannelCommand {
    List,
    Weixin {
        #[command(subcommand)]
        command: WeixinCommand,
    },
}

#[derive(Subcommand)]
enum WeixinCommand {
    Status,
    Bind,
    Unbind,
    SendMessage { message: String },
    SendFile { path: PathBuf },
}

#[derive(Subcommand)]
enum McpCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Search {
        #[command(subcommand)]
        command: McpSearchCommand,
    },
    Show {
        selector: String,
        #[arg(long)]
        json: bool,
    },
    Call {
        selector: String,
        #[arg(long, default_value = "{}")]
        args: String,
        #[arg(long)]
        json: bool,
    },
    Auth {
        action_or_server: String,
        server: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum McpSearchCommand {
    Query {
        query: String,
        #[arg(long)]
        json: bool,
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
        McpCommand::List { json } => {
            let value = ipc::request(config_path, "mcp.list", json!({})).await?;
            if json {
                print_value(&value)?;
            } else {
                let catalog: Catalog = serde_json::from_value(value)?;
                println!("{}", dwo_mcp::render_list(&catalog));
            }
        }
        McpCommand::Search {
            command: McpSearchCommand::Query { query, json },
        } => {
            let value = ipc::request(config_path, "mcp.search", json!({"query": query})).await?;
            if json {
                print_value(&value)?;
            } else {
                let groups: Vec<SearchGroup> = serde_json::from_value(value)?;
                println!("{}", dwo_mcp::render_search(&groups));
            }
        }
        McpCommand::Show { selector, json } => {
            let value =
                ipc::request(config_path, "mcp.show", json!({"selector": selector})).await?;
            if json {
                print_value(&value)?;
            } else if value["kind"] == "server" {
                let server: CatalogServer = serde_json::from_value(value["server"].clone())?;
                println!("{}", dwo_mcp::render_show(ShowResult::Server(&server)));
            } else {
                let server: CatalogServer = serde_json::from_value(value["server"].clone())?;
                let tool: CatalogTool = serde_json::from_value(value["tool"].clone())?;
                println!(
                    "{}",
                    dwo_mcp::render_show(ShowResult::Tool {
                        server: &server,
                        tool: &tool,
                    })
                );
            }
        }
        McpCommand::Call {
            selector,
            args,
            json: _,
        } => {
            let arguments: Value = serde_json::from_str(&args).context("parse --args JSON")?;
            let value = ipc::request(
                config_path,
                "mcp.call",
                json!({"selector": selector, "arguments": arguments}),
            )
            .await?;
            print_value(&value)?;
        }
        McpCommand::Auth {
            action_or_server,
            server,
            json,
        } => {
            let (method, server) = match (action_or_server.as_str(), server) {
                ("status", Some(server)) => ("mcp.auth.status", server),
                ("logout", Some(server)) => ("mcp.auth.logout", server),
                ("status" | "logout", None) => {
                    bail!("usage: dwo mcp auth {} <server>", action_or_server)
                }
                (_, Some(_)) => {
                    bail!("usage: dwo mcp auth <server>|status <server>|logout <server>")
                }
                (server, None) => {
                    println!("Opening the authorization page for {server}...");
                    ("mcp.auth.login", server.to_string())
                }
            };
            let value = ipc::request(config_path, method, json!({"server": server})).await?;
            if json {
                print_value(&value)?;
            } else if method == "mcp.auth.status" {
                println!("{}", value.as_str().unwrap_or("unknown"));
            } else {
                println!("Authorization updated");
            }
        }
    }
    Ok(())
}

async fn run_session(command: SessionCommand, config_path: &Path) -> Result<()> {
    let endpoint_id = format!("cli-{}", Uuid::new_v4());
    match command {
        SessionCommand::List => {
            let value = ipc::request(config_path, "session.list", json!({})).await?;
            print_value(&value)?;
        }
        SessionCommand::New { name, cwd } => {
            let value = ipc::request(
                config_path,
                "session.new",
                json!({"title": name, "cwd": cwd}),
            )
            .await?;
            print_value(&value)?;
        }
        SessionCommand::Delete { id } => {
            ipc::request(config_path, "session.delete", json!({"session_id": id})).await?;
            println!("Deleted session");
        }
        SessionCommand::Prompt { id, message } => {
            let value = ipc::request(
                config_path,
                "session.prompt",
                json!({
                    "session_id": id,
                    "endpoint_id": endpoint_id,
                    "message": message,
                }),
            )
            .await?;
            print_value(&value)?;
        }
        SessionCommand::Cancel { id } => {
            ipc::request(config_path, "session.cancel", json!({"session_id": id})).await?;
            println!("Cancellation requested");
        }
        SessionCommand::Watch { id } => ipc::watch(config_path, &id, &endpoint_id).await?,
        SessionCommand::Model { id, model } => {
            ipc::request(
                config_path,
                "session.set_model",
                json!({"session_id": id, "value": model}),
            )
            .await?;
            println!("Model updated");
        }
        SessionCommand::Reasoning { id, reasoning } => {
            ipc::request(
                config_path,
                "session.set_reasoning",
                json!({"session_id": id, "value": reasoning}),
            )
            .await?;
            println!("Reasoning updated");
        }
        SessionCommand::Approve { id, permission_id } => {
            permission(config_path, id, endpoint_id, permission_id, true, None).await?;
        }
        SessionCommand::Deny {
            id,
            permission_id,
            reason,
        } => {
            permission(config_path, id, endpoint_id, permission_id, false, reason).await?;
        }
    }
    Ok(())
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
        ChannelCommand::Weixin { command } => run_weixin(command, config_path).await?,
    }
    Ok(())
}

async fn run_weixin(command: WeixinCommand, config_path: &Path) -> Result<()> {
    match command {
        WeixinCommand::Status => {
            let value = ipc::request(config_path, "channel.weixin.status", json!({})).await?;
            print_value(&value)?;
        }
        WeixinCommand::Unbind => {
            let value = ipc::request(config_path, "channel.weixin.remove", json!({})).await?;
            print_value(&value)?;
        }
        WeixinCommand::SendMessage { message } => {
            let value = ipc::request(
                config_path,
                "channel.weixin.send_message",
                json!({"text": message}),
            )
            .await?;
            print_value(&value)?;
        }
        WeixinCommand::SendFile { path } => {
            let value = ipc::request(
                config_path,
                "channel.weixin.send_file",
                json!({"path": path}),
            )
            .await?;
            print_value(&value)?;
        }
        WeixinCommand::Bind => {
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
    for _ in 0..50 {
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
    std::fs::create_dir_all(root.join("resource/prompts"))?;
    std::fs::create_dir_all(root.join("resource/skills"))?;
    std::fs::create_dir_all(root.join("runtime/sessions"))?;
    std::fs::create_dir_all(root.join("mcp_runtime"))?;
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
    register_service(config_path)
}

fn write_if_missing(path: &Path, content: &str) -> Result<()> {
    if !path.exists() {
        std::fs::write(path, content)?;
    }
    Ok(())
}

#[cfg(windows)]
fn register_service(config_path: &Path) -> Result<()> {
    let executable = std::env::current_exe()?;
    let command = format!(
        "\"{}\" --config-path \"{}\" serve",
        executable.display(),
        config_path.display()
    );
    let root = config_path.parent().context("config path has no parent")?;
    let launcher = root.join("runtime/dwo-daemon.vbs");
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
fn register_service(config_path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let executable = std::env::current_exe()?;
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
fn register_service(_config_path: &Path) -> Result<()> {
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
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

const DEFAULT_PROFILE: &str = r#"name: coder
description: coding agent
policyMode: confirm
channels:
  weixin:
    enabled: true
    streamMode: answer
    replayTurns: 5
    markdownFilter: true
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
                    command: WeixinCommand::Status
                }
            }
        ));

        let send =
            Cli::try_parse_from(["dwo", "channel", "weixin", "send-file", "report.pdf"]).unwrap();
        assert!(matches!(
            send.command,
            Command::Channel {
                command: ChannelCommand::Weixin {
                    command: WeixinCommand::SendFile { ref path }
                }
            } if path == &PathBuf::from("report.pdf")
        ));
    }

    #[test]
    fn parses_progressive_mcp_commands() {
        let search =
            Cli::try_parse_from(["dwo", "mcp", "search", "query", "install", "--json"]).unwrap();
        assert!(matches!(
            search.command,
            Command::Mcp {
                command: McpCommand::Search {
                    command: McpSearchCommand::Query {
                        ref query,
                        json: true,
                    }
                }
            } if query == "install"
        ));

        let auth = Cli::try_parse_from(["dwo", "mcp", "auth", "status", "github"]).unwrap();
        assert!(matches!(
            auth.command,
            Command::Mcp {
                command: McpCommand::Auth {
                    ref action_or_server,
                    server: Some(ref server),
                    json: false,
                }
            } if action_or_server == "status" && server == "github"
        ));
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
}
