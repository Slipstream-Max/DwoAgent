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
use crate::logging;

mod install;
mod output;
mod render;

use install::{install, unregister_service};

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
    Websocket {
        #[command(subcommand)]
        command: WebsocketChannelCommand,
    },
}

#[derive(Subcommand)]
enum WebsocketChannelCommand {
    Status,
    Token,
    ResetToken,
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
            output::line(format_args!(
                "Installed profile at {}",
                config_path.display()
            ))?;
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
            output::line(format_args!(
                "Uninstalled dwoagent{}",
                if purge { " and removed profile" } else { "" }
            ))?;
        }
        Command::Serve => {
            run_daemon(&config_path).await?;
        }
        Command::Daemon { command } => match command {
            DaemonCommand::Start => daemon_start(&config_path).await?,
            DaemonCommand::Stop => {
                ipc::request(&config_path, "daemon.shutdown", json!({})).await?;
                output::line(format_args!("Stopping dwoagent daemon"))?;
            }
            DaemonCommand::Status => {
                let status = ipc::request(&config_path, "daemon.status", json!({})).await?;
                render::write_value(&status)?;
            }
        },
        Command::Session { command } => run_session(command, &config_path).await?,
        Command::ProfileList => {
            let value = ipc::request(&config_path, "profile.list", json!({})).await?;
            render::write_value(&value)?;
        }
        Command::Channel { command } => run_channel(command, &config_path).await?,
        Command::Mcp { command } => run_mcp(command, &config_path).await?,
        Command::Automation { command } => run_automation(command, &config_path).await?,
        Command::Acp => acp::run(config_path).await?,
    }
    Ok(())
}

async fn run_daemon(config_path: &Path) -> Result<()> {
    let _logging = logging::init(config_path)?;
    tracing::info!(
        event = "daemon.starting",
        config_path = %config_path.display(),
        "daemon starting"
    );
    let result = async {
        let host = host::Host::load(config_path).await?;
        tracing::info!(
            event = "daemon.ready",
            endpoint = %ipc::endpoint(config_path),
            "daemon ready"
        );
        ipc::serve(host, config_path).await
    }
    .await;
    match &result {
        Ok(()) => tracing::info!(event = "daemon.stopped", "daemon stopped"),
        Err(error) => tracing::error!(
            event = "daemon.failed",
            error = %format!("{error:#}"),
            "daemon failed"
        ),
    }
    result
}

async fn run_automation(command: AutomationCommand, config_path: &Path) -> Result<()> {
    match command {
        AutomationCommand::List { json } | AutomationCommand::Status { json } => {
            let value = ipc::request(config_path, "automation.list", json!({})).await?;
            if json {
                render::write_value(&value)?;
            } else {
                let jobs: Vec<AutomationJobStatus> = serde_json::from_value(value)?;
                if jobs.is_empty() {
                    output::line(format_args!("No automation jobs configured"))?;
                }
                for status in jobs {
                    let next = status.next_run_at.as_deref().unwrap_or("disabled");
                    let active = if status.active_runs.is_empty() {
                        String::new()
                    } else {
                        format!(" active={}", status.active_runs.len())
                    };
                    output::line(format_args!("{}  next={}{}", status.job.name, next, active))?;
                }
            }
        }
        AutomationCommand::Run { job, json } => {
            let value = ipc::request(config_path, "automation.run", json!({"job": job})).await?;
            if json {
                render::write_value(&value)?;
            } else {
                let record: AutomationRunRecord = serde_json::from_value(value)?;
                output::line(format_args!(
                    "{}  {:?}  session={}",
                    record.job,
                    record.status,
                    record.session_id.as_deref().unwrap_or("-")
                ))?;
                if let Some(error) = record.error {
                    output::line(format_args!("error: {error}"))?;
                }
                if !record.response.is_empty() {
                    output::line(format_args!("\n{}", record.response))?;
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
            output::write(format_args!("{}", dwo_mcp::render_search(&groups)))?;
        }
        McpCommand::Call { selector, args } => {
            let arguments: Value = serde_json::from_str(&args).context("parse --args JSON")?;
            let value = ipc::request(
                config_path,
                "mcp.call",
                json!({"selector": selector, "arguments": arguments}),
            )
            .await?;
            render::write_value(&value)?;
        }
        McpCommand::Auth { server, logout } => {
            let method = if logout {
                "mcp.auth.logout"
            } else {
                output::line(format_args!(
                    "Opening the authorization page for {server}..."
                ))?;
                "mcp.auth.login"
            };
            ipc::request(config_path, method, json!({"server": server})).await?;
            output::line(format_args!(
                "Authorization {}",
                if logout { "removed" } else { "updated" }
            ))?;
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
            render::write_session_list(&value)?;
        }
        SessionCommand::Delete { id } => {
            ipc::request(config_path, "session.delete", json!({"session_id": id})).await?;
            output::line(format_args!("Deleted session"))?;
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
            render::write_value(&value)?;
        }
        SessionCommand::Cancel { id } => {
            ipc::request(config_path, "session.cancel", json!({"session_id": id})).await?;
            output::line(format_args!("Cancellation requested"))?;
        }
        SessionCommand::Watch { id, cursor, limit } => {
            let value = ipc::request(
                config_path,
                "session.read",
                json!({"session_id": id, "cursor": cursor, "limit": limit}),
            )
            .await?;
            render::write_value(&value)?;
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
    output::line(format_args!("Permission resolved"))?;
    Ok(())
}

async fn run_channel(command: ChannelCommand, config_path: &Path) -> Result<()> {
    match command {
        ChannelCommand::List => {
            let value = ipc::request(config_path, "channel.list", json!({})).await?;
            render::write_value(&value)?;
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
        ChannelCommand::Websocket { command } => {
            let action = match command {
                WebsocketChannelCommand::Status => "status",
                WebsocketChannelCommand::Token => "token",
                WebsocketChannelCommand::ResetToken => "reset_token",
            };
            let value = ipc::request(
                config_path,
                &format!("channel.websocket.{action}"),
                json!({}),
            )
            .await?;
            render::write_value(&value)?;
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
            render::write_value(&value)?;
        }
        ManagedChannelCommand::Unbind => {
            let value = ipc::request(config_path, &method("remove"), json!({})).await?;
            render::write_value(&value)?;
        }
        ManagedChannelCommand::SendMessage { message } => {
            let value = ipc::request(
                config_path,
                &method("send_message"),
                json!({"text": message}),
            )
            .await?;
            render::write_value(&value)?;
        }
        ManagedChannelCommand::SendFile { path } => {
            let value =
                ipc::request(config_path, &method("send_file"), json!({"path": path})).await?;
            render::write_value(&value)?;
        }
        ManagedChannelCommand::Bind => match channel {
            ChannelKind::Weixin => bind_weixin(config_path).await?,
            ChannelKind::Telegram => bind_telegram(config_path).await?,
            ChannelKind::Feishu => bind_feishu(config_path).await?,
            ChannelKind::Websocket => bail!("WebSocket channel does not use binding"),
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
    output::line(format_args!("Scan this QR code with Weixin:\n"))?;
    let rendered_qr = qr2term::generate_qr_string(qrcode).unwrap_or_else(|_| qrcode.to_string());
    output::line(format_args!("{rendered_qr}"))?;
    let mut verify_code: Option<String> = None;
    loop {
        tokio::time::sleep(channels::BIND_POLL_INTERVAL).await;
        let progress = ipc::request(
            config_path,
            "channel.weixin.poll",
            json!({"binding_id": binding_id, "verify_code": verify_code.take()}),
        )
        .await?;
        let progress: WeixinLoginProgress = serde_json::from_value(progress)?;
        match progress {
            WeixinLoginProgress::Waiting => {}
            WeixinLoginProgress::Scanned => {
                output::line(format_args!("Scanned; confirm on your phone"))?;
            }
            WeixinLoginProgress::Confirmed { channel } => {
                output::line(format_args!("Channel {} connected", channel.name))?;
                break;
            }
            WeixinLoginProgress::NeedVerifyCode => {
                output::line(format_args!(
                    "Enter the verification code shown on your phone:"
                ))?;
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
        output::line(format_args!("dwoagent daemon is already running"))?;
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
            output::line(format_args!("dwoagent daemon started"))?;
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

fn default_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".dwoagent/profile.yaml"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .context("cannot determine user home directory")
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
    output::line(format_args!(
        "Open @{bot_username} in Telegram and send this private message:\n"
    ))?;
    output::line(format_args!("/bind {code}\n"))?;
    output::line(format_args!("Waiting for Telegram binding confirmation..."))?;
    loop {
        tokio::time::sleep(channels::BIND_POLL_INTERVAL).await;
        let progress = ipc::request(
            config_path,
            "channel.telegram.poll",
            json!({"binding_id": binding_id}),
        )
        .await?;
        match serde_json::from_value::<TelegramBindProgress>(progress)? {
            TelegramBindProgress::Waiting => {}
            TelegramBindProgress::Confirmed { channel } => {
                output::line(format_args!("Channel {} connected", channel.name))?;
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
    output::line(format_args!(
        "Open the application bot in {product} and send this private message:\n"
    ))?;
    output::line(format_args!("/bind {code}\n"))?;
    output::line(format_args!(
        "Waiting for {product} binding confirmation..."
    ))?;
    loop {
        tokio::time::sleep(channels::BIND_POLL_INTERVAL).await;
        let progress = ipc::request(
            config_path,
            "channel.feishu.poll",
            json!({"binding_id": binding_id}),
        )
        .await?;
        match serde_json::from_value::<FeishuBindProgress>(progress)? {
            FeishuBindProgress::Waiting => {}
            FeishuBindProgress::Confirmed { channel } => {
                output::line(format_args!("Channel {} connected", channel.name))?;
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
logging:
  level: info
  retentionDays: 14
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
  websocket:
    enabled: false
    port: 8765
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
    fn parses_websocket_commands() {
        let token = Cli::try_parse_from(["dwo", "channel", "websocket", "token"]).unwrap();
        assert!(matches!(
            token.command,
            Command::Channel {
                command: ChannelCommand::Websocket {
                    command: WebsocketChannelCommand::Token
                }
            }
        ));

        let reset = Cli::try_parse_from(["dwo", "channel", "websocket", "reset-token"]).unwrap();
        assert!(matches!(
            reset.command,
            Command::Channel {
                command: ChannelCommand::Websocket {
                    command: WebsocketChannelCommand::ResetToken
                }
            }
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
