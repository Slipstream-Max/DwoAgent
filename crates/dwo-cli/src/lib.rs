use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use dwo_mcp::SearchGroup;
use serde_json::{Value, json};
use uuid::Uuid;

use dwo_acp as acp;
use dwo_channels::{
    self, ChannelKind, FeishuBindProgress, QqBindProgress, TelegramBindProgress,
    WeixinLoginProgress,
};
use dwo_host::automation::{
    AutomationJob, AutomationJobStatus, AutomationNewBehavior, AutomationRunRecord,
    AutomationSchedule, AutomationSession,
};
use dwo_ipc as ipc;

mod install;
mod output;
mod render;

use install::{install, unregister_service};

#[derive(Parser)]
#[command(name = "dwo", version, about = "dwoagent host and control CLI")]
struct Cli {
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
    ConfigShow,
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
    Websocket {
        #[command(subcommand)]
        command: WebsocketCommand,
    },
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    Automation {
        #[command(subcommand)]
        command: AutomationCommand,
    },
    Acp {
        #[arg(long, value_enum, default_value_t = acp::AcpProtocol::V2)]
        protocol: acp::AcpProtocol,
    },
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
    Keep {
        id: String,
    },
    Status {
        id: String,
        #[arg(long)]
        json: bool,
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
        #[arg(long, conflicts_with_all = ["to", "from"])]
        ephemeral: bool,
        #[arg(long, conflicts_with = "from")]
        to: Option<String>,
        #[arg(long, conflicts_with = "to")]
        from: Option<String>,
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
    Qq {
        #[command(subcommand)]
        command: ManagedChannelCommand,
    },
}

#[derive(Subcommand)]
enum WebsocketCommand {
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
    /// List configured MCP server names and their current status.
    List,
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
        job: String,
        #[arg(long)]
        json: bool,
    },
    Add {
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long, default_value = "local")]
        timezone: String,
        #[arg(long)]
        prompt: String,
        #[arg(long, value_enum, default_value_t = AutomationSessionArg::EveryTime)]
        session: AutomationSessionArg,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        disabled: bool,
        #[arg(long)]
        json: bool,
    },
    Enable {
        job: Option<String>,
        #[arg(long, conflicts_with = "job", required_unless_present = "job")]
        all: bool,
    },
    Disable {
        job: Option<String>,
        #[arg(long, conflicts_with = "job", required_unless_present = "job")]
        all: bool,
    },
    #[command(alias = "del")]
    Delete {
        job: Option<String>,
        #[arg(long, conflicts_with = "job", required_unless_present = "job")]
        all: bool,
        #[arg(long, requires = "all")]
        yes: bool,
    },
    Run {
        job: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AutomationSessionArg {
    EveryTime,
    Once,
    Fixed,
}

pub async fn run<S, Fut>(serve: S) -> Result<()>
where
    S: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let cli = Cli::parse();
    let config_path = default_config_path()?;
    match cli.command {
        Command::Install { start } => {
            let restart = stop_daemon_for_upgrade(&config_path).await?;
            install(&config_path)?;
            output::line(format_args!(
                "Installed profile at {}",
                config_path.display()
            ))?;
            if start || restart {
                daemon_start(&config_path).await?;
            }
        }
        Command::Uninstall { purge } => {
            let _ = ipc::request_dwo(&config_path, "daemon.shutdown", json!({})).await;
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
            serve(config_path).await?;
        }
        Command::Daemon { command } => match command {
            DaemonCommand::Start => daemon_start(&config_path).await?,
            DaemonCommand::Stop => {
                ipc::request_dwo(&config_path, "daemon.shutdown", json!({})).await?;
                output::line(format_args!("Stopping dwoagent daemon"))?;
            }
            DaemonCommand::Status => {
                let status = ipc::request_dwo(&config_path, "daemon.status", json!({})).await?;
                render::write_value(&status)?;
            }
        },
        Command::Session { command } => run_session(command, &config_path).await?,
        Command::ConfigShow => {
            let value = ipc::request_dwo(&config_path, "config.snapshot", json!({})).await?;
            render::write_value(&value)?;
        }
        Command::Channel { command } => run_channel(command, &config_path).await?,
        Command::Websocket { command } => run_websocket(command, &config_path).await?,
        Command::Mcp { command } => run_mcp(command, &config_path).await?,
        Command::Automation { command } => run_automation(command, &config_path).await?,
        Command::Acp { protocol } => acp::run(config_path, protocol).await?,
    }
    Ok(())
}

async fn run_automation(command: AutomationCommand, config_path: &Path) -> Result<()> {
    match command {
        AutomationCommand::List { json } => {
            let value = ipc::request_dwo(config_path, "automation.list", json!({})).await?;
            if json {
                render::write_value(&value)?;
            } else {
                let jobs: Vec<AutomationJobStatus> = serde_json::from_value(value)?;
                render::write_automation_list(&jobs)?;
            }
        }
        AutomationCommand::Status { job, json } => {
            let value =
                ipc::request_dwo(config_path, "automation.status", json!({"job": job})).await?;
            if json {
                render::write_value(&value)?;
            } else {
                let status: AutomationJobStatus = serde_json::from_value(value)?;
                render::write_automation_status(&status)?;
            }
        }
        AutomationCommand::Add {
            name,
            cron,
            timezone,
            prompt,
            session,
            session_id,
            cwd,
            title,
            disabled,
            json,
        } => {
            let session = match session {
                AutomationSessionArg::EveryTime | AutomationSessionArg::Once => {
                    anyhow::ensure!(
                        session_id.is_none(),
                        "--session-id requires --session fixed"
                    );
                    AutomationSession::New {
                        behavior: if matches!(session, AutomationSessionArg::EveryTime) {
                            AutomationNewBehavior::EveryTime
                        } else {
                            AutomationNewBehavior::Once
                        },
                        cwd: cwd.unwrap_or_else(|| PathBuf::from(".")),
                        title,
                    }
                }
                AutomationSessionArg::Fixed => {
                    anyhow::ensure!(cwd.is_none(), "--cwd is unavailable with --session fixed");
                    anyhow::ensure!(
                        title.is_none(),
                        "--title is unavailable with --session fixed"
                    );
                    AutomationSession::Fixed {
                        session_id: session_id
                            .context("--session-id is required with --session fixed")?,
                    }
                }
            };
            let job = AutomationJob {
                name,
                enabled: !disabled,
                schedule: AutomationSchedule { cron, timezone },
                session,
                prompt,
                model: None,
                reasoning: None,
                policy: None,
            };
            let value =
                ipc::request_dwo(config_path, "automation.add", json!({"job": job})).await?;
            if json {
                render::write_value(&value)?;
            } else {
                let status: AutomationJobStatus = serde_json::from_value(value)?;
                output::line(format_args!("Added automation {}", status.job.name))?;
            }
        }
        AutomationCommand::Enable { job, all } => {
            set_automation_enabled(config_path, job, all, true).await?;
        }
        AutomationCommand::Disable { job, all } => {
            set_automation_enabled(config_path, job, all, false).await?;
        }
        AutomationCommand::Delete { job, all, yes } => {
            anyhow::ensure!(!all || yes, "automation delete --all requires --yes");
            ipc::request_dwo(
                config_path,
                "automation.delete",
                json!({"job": job, "all": all}),
            )
            .await?;
            output::line(format_args!(
                "Deleted automation {}",
                if all {
                    "jobs".to_string()
                } else {
                    job.unwrap()
                }
            ))?;
        }
        AutomationCommand::Run { job, json } => {
            let value = ipc::request_dwo(
                config_path,
                "automation.run",
                json!({"job": job, "caller_session_id": current_session_id()}),
            )
            .await?;
            if json {
                render::write_value(&value)?;
            } else {
                let record: AutomationRunRecord = serde_json::from_value(value)?;
                output::line(format_args!(
                    "Started automation {}  run={}  session={}  turn={}",
                    record.job,
                    record.run_id,
                    record.session_id.as_deref().unwrap_or("-"),
                    record.turn_id.as_deref().unwrap_or("-")
                ))?;
            }
        }
    }
    Ok(())
}

async fn set_automation_enabled(
    config_path: &Path,
    job: Option<String>,
    all: bool,
    enabled: bool,
) -> Result<()> {
    let method = if enabled {
        "automation.enable"
    } else {
        "automation.disable"
    };
    ipc::request_dwo(config_path, method, json!({"job": job, "all": all})).await?;
    output::line(format_args!(
        "{} automation {}",
        if enabled { "Enabled" } else { "Disabled" },
        if all {
            "jobs".to_string()
        } else {
            job.expect("clap requires a job unless --all is present")
        }
    ))?;
    Ok(())
}

async fn run_mcp(command: McpCommand, config_path: &Path) -> Result<()> {
    match command {
        McpCommand::List => {
            let value = ipc::request_dwo(config_path, "mcp.list", json!({})).await?;
            let catalog: dwo_mcp::Catalog = serde_json::from_value(value)?;
            output::line(format_args!("{}", dwo_mcp::render_list(&catalog)))?;
        }
        McpCommand::Search { query } => {
            let value =
                ipc::request_dwo(config_path, "mcp.search", json!({"query": query})).await?;
            let groups: Vec<SearchGroup> = serde_json::from_value(value)?;
            output::write(format_args!("{}", dwo_mcp::render_search(&groups)))?;
        }
        McpCommand::Call { selector, args } => {
            let arguments: Value = serde_json::from_str(&args).context("parse --args JSON")?;
            let value = ipc::request_dwo(
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
            ipc::request_dwo(config_path, method, json!({"server": server})).await?;
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
            let value = ipc::request_dwo(
                config_path,
                "session.status-list",
                json!({"all": all, "caller_session_id": current_session_id()}),
            )
            .await?;
            render::write_session_list(&value)?;
        }
        SessionCommand::Delete { id } => {
            ipc::request_dwo(config_path, "session.delete", json!({"session_id": id})).await?;
            output::line(format_args!("Deleted session"))?;
        }
        SessionCommand::Keep { id } => {
            let value =
                ipc::request_dwo(config_path, "session.keep", json!({"session_id": id})).await?;
            output::line(format_args!(
                "{}",
                if value["changed"] == true {
                    "Session kept"
                } else {
                    "Session already persistent"
                }
            ))?;
        }
        SessionCommand::Status { id, json } => {
            let value =
                ipc::request_dwo(config_path, "session.status", json!({"session_id": id})).await?;
            if json {
                render::write_value(&value)?;
            } else {
                render::write_session_status(&value)?;
            }
        }
        SessionCommand::Prompt {
            message,
            title,
            cwd,
            policy,
            model,
            reasoning,
            to,
            from,
            ephemeral,
        } => {
            let policy = policy
                .map(|value| dwo_tools::SessionMode::parse(&value).map_err(anyhow::Error::msg))
                .transpose()?;
            let value = ipc::request_acp(
                config_path,
                "session.prompt",
                json!({
                    "session_id": to,
                    "from_session_id": from,
                    "caller_session_id": current_session_id(),
                    "endpoint_id": endpoint_id,
                    "message": message,
                    "title": title,
                    "cwd": cwd,
                    "policy": policy,
                    "model": model,
                    "reasoning": reasoning,
                    "ephemeral": ephemeral,
                }),
            )
            .await?;
            render::write_value(&value)?;
        }
        SessionCommand::Cancel { id } => {
            ipc::request_acp(config_path, "session.cancel", json!({"session_id": id})).await?;
            output::line(format_args!("Cancellation requested"))?;
        }
        SessionCommand::Watch { id, cursor, limit } => {
            let value = ipc::request_dwo(
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
    ipc::request_acp(
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
            let value = ipc::request_dwo(config_path, "channel.list", json!({})).await?;
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
        ChannelCommand::Qq { command } => {
            run_managed_channel(ChannelKind::Qq, command, config_path).await?
        }
    }
    Ok(())
}

async fn run_websocket(command: WebsocketCommand, config_path: &Path) -> Result<()> {
    let method = match command {
        WebsocketCommand::Status => "websocket.status",
        WebsocketCommand::Token => "websocket.token",
        WebsocketCommand::ResetToken => "websocket.reset_token",
    };
    let value = ipc::request_dwo(config_path, method, json!({})).await?;
    render::write_value(&value)?;
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
            let value = ipc::request_dwo(config_path, &method("status"), json!({})).await?;
            render::write_value(&value)?;
        }
        ManagedChannelCommand::Unbind => {
            let value = ipc::request_dwo(config_path, &method("remove"), json!({})).await?;
            render::write_value(&value)?;
        }
        ManagedChannelCommand::SendMessage { message } => {
            let value = ipc::request_dwo(
                config_path,
                &method("send_message"),
                json!({"text": message}),
            )
            .await?;
            render::write_value(&value)?;
        }
        ManagedChannelCommand::SendFile { path } => {
            let value =
                ipc::request_dwo(config_path, &method("send_file"), json!({"path": path})).await?;
            render::write_value(&value)?;
        }
        ManagedChannelCommand::Bind => match channel {
            ChannelKind::Weixin => bind_weixin(config_path).await?,
            ChannelKind::Telegram => bind_telegram(config_path).await?,
            ChannelKind::Feishu => bind_feishu(config_path).await?,
            ChannelKind::Qq => bind_qq(config_path).await?,
        },
    }
    Ok(())
}

async fn bind_weixin(config_path: &Path) -> Result<()> {
    let start = ipc::request_dwo(config_path, "channel.weixin.begin", json!({})).await?;
    let binding_id = start["binding_id"]
        .as_str()
        .context("daemon omitted binding_id")?;
    let qrcode = start["qrcode"].as_str().context("daemon omitted qrcode")?;
    output::line(format_args!("Scan this QR code with Weixin:\n"))?;
    let rendered_qr = qr2term::generate_qr_string(qrcode).unwrap_or_else(|_| qrcode.to_string());
    output::line(format_args!("{rendered_qr}"))?;
    let mut verify_code: Option<String> = None;
    loop {
        tokio::time::sleep(dwo_channels::BIND_POLL_INTERVAL).await;
        let progress = ipc::request_dwo(
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
    if ipc::request_dwo(config_path, "daemon.status", json!({}))
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
        if ipc::request_dwo(config_path, "daemon.status", json!({}))
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

async fn stop_daemon_for_upgrade(config_path: &Path) -> Result<bool> {
    if ipc::request_dwo(config_path, "daemon.status", json!({}))
        .await
        .is_err()
    {
        return Ok(false);
    }
    let _ = ipc::request_dwo(config_path, "daemon.shutdown", json!({})).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if ipc::request_dwo(config_path, "daemon.status", json!({}))
            .await
            .is_err()
        {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("daemon did not stop within 30 seconds")
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
    let start = ipc::request_dwo(config_path, "channel.telegram.begin", json!({})).await?;
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
        tokio::time::sleep(dwo_channels::BIND_POLL_INTERVAL).await;
        let progress = ipc::request_dwo(
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
    let start = ipc::request_dwo(config_path, "channel.feishu.begin", json!({})).await?;
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
        tokio::time::sleep(dwo_channels::BIND_POLL_INTERVAL).await;
        let progress = ipc::request_dwo(
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

async fn bind_qq(config_path: &Path) -> Result<()> {
    let start = ipc::request_dwo(config_path, "channel.qq.begin", json!({})).await?;
    let binding_id = start["binding_id"]
        .as_str()
        .context("daemon omitted binding_id")?;
    let qrcode = start["qrcode"].as_str().context("daemon omitted qrcode")?;
    output::line(format_args!("Scan this QR code with QQ:\n"))?;
    let rendered_qr = qr2term::generate_qr_string(qrcode).unwrap_or_else(|_| qrcode.to_string());
    output::line(format_args!("{rendered_qr}"))?;
    output::line(format_args!("Waiting for QQ binding confirmation..."))?;
    loop {
        tokio::time::sleep(dwo_channels::BIND_POLL_INTERVAL).await;
        let progress = ipc::request_dwo(
            config_path,
            "channel.qq.poll",
            json!({"binding_id": binding_id}),
        )
        .await?;
        match serde_json::from_value::<QqBindProgress>(progress)? {
            QqBindProgress::Waiting => {}
            QqBindProgress::Confirmed { channel } => {
                output::line(format_args!("Channel {} connected", channel.name))?;
                break;
            }
            QqBindProgress::Expired => bail!("QQ QR code expired"),
            QqBindProgress::Failed { message } => bail!(message),
        }
    }
    Ok(())
}

const DEFAULT_PROFILE: &str = r#"policyMode: confirm
maxModelSteps: 100
externalSkillsDirs: []
logging:
  level: info
  retentionDays: 14
channels:
  weixin:
    enabled: true
    replayTurns: 5
    outputMode: final
    markdownFilter: true
    mediaInput: true
  telegram:
    enabled: false
    replayTurns: 5
    outputMode: final
    botTokenEnv: TELEGRAM_BOT_TOKEN
    tgProxy: null
    mediaInput: true
  feishu:
    enabled: false
    replayTurns: 5
    outputMode: final
    appIdEnv: FEISHU_APP_ID
    appSecretEnv: FEISHU_APP_SECRET
    platform: feishu
    mediaInput: true
  qq:
    enabled: false
    replayTurns: 5
    outputMode: final
    mediaInput: true
websocket:
  enabled: false
  bind: 127.0.0.1
  port: 8787
automation:
  enabled: false
  timeoutSeconds: 900
  jobs: []
model:
  default:
    model: deepseek/deepseek-v4-pro
  providers:
    deepseek:
      apiKeyEnv: DEEPSEEK_API_KEY
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_acp_protocol_version() {
        let default = Cli::try_parse_from(["dwo", "acp"]).unwrap();
        assert!(matches!(
            default.command,
            Command::Acp {
                protocol: acp::AcpProtocol::V2
            }
        ));

        let v1 = Cli::try_parse_from(["dwo", "acp", "--protocol", "v1"]).unwrap();
        assert!(matches!(
            v1.command,
            Command::Acp {
                protocol: acp::AcpProtocol::V1
            }
        ));
    }

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
    fn parses_qq_commands() {
        let bind = Cli::try_parse_from(["dwo", "channel", "qq", "bind"]).unwrap();
        assert!(matches!(
            bind.command,
            Command::Channel {
                command: ChannelCommand::Qq {
                    command: ManagedChannelCommand::Bind
                }
            }
        ));

        let send =
            Cli::try_parse_from(["dwo", "channel", "qq", "send-file", "report.zip"]).unwrap();
        assert!(matches!(
            send.command,
            Command::Channel {
                command: ChannelCommand::Qq {
                    command: ManagedChannelCommand::SendFile { ref path }
                }
            } if path == &PathBuf::from("report.zip")
        ));
    }

    #[test]
    fn parses_websocket_commands() {
        let token = Cli::try_parse_from(["dwo", "websocket", "token"]).unwrap();
        assert!(matches!(
            token.command,
            Command::Websocket {
                command: WebsocketCommand::Token
            }
        ));

        let reset = Cli::try_parse_from(["dwo", "websocket", "reset-token"]).unwrap();
        assert!(matches!(
            reset.command,
            Command::Websocket {
                command: WebsocketCommand::ResetToken
            }
        ));
    }

    #[test]
    fn parses_minimal_mcp_commands() {
        let list = Cli::try_parse_from(["dwo", "mcp", "list"]).unwrap();
        assert!(matches!(
            list.command,
            Command::Mcp {
                command: McpCommand::List
            }
        ));

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

        let add = Cli::try_parse_from([
            "dwo",
            "automation",
            "add",
            "daily-report",
            "--cron",
            "0 9 * * *",
            "--prompt",
            "summarize",
            "--session",
            "once",
        ])
        .unwrap();
        assert!(matches!(
            add.command,
            Command::Automation {
                command: AutomationCommand::Add {
                    ref name,
                    session: AutomationSessionArg::Once,
                    ..
                }
            } if name == "daily-report"
        ));

        let enable_all = Cli::try_parse_from(["dwo", "automation", "enable", "--all"]).unwrap();
        assert!(matches!(
            enable_all.command,
            Command::Automation {
                command: AutomationCommand::Enable {
                    job: None,
                    all: true
                }
            }
        ));
        assert!(Cli::try_parse_from(["dwo", "automation", "enable"]).is_err());
        assert!(
            Cli::try_parse_from(["dwo", "automation", "enable", "daily-report", "--all"]).is_err()
        );

        let status = Cli::try_parse_from(["dwo", "session", "status", "session-1"]).unwrap();
        assert!(matches!(
            status.command,
            Command::Session {
                command: SessionCommand::Status { ref id, json: false }
            } if id == "session-1"
        ));
    }

    #[test]
    fn parses_subsession_commands() {
        let ephemeral =
            Cli::try_parse_from(["dwo", "session", "prompt", "inspect once", "--ephemeral"])
                .unwrap();
        assert!(matches!(
            ephemeral.command,
            Command::Session {
                command: SessionCommand::Prompt {
                    ephemeral: true,
                    to: None,
                    from: None,
                    ..
                }
            }
        ));

        let keep = Cli::try_parse_from(["dwo", "session", "keep", "session-child"]).unwrap();
        assert!(matches!(
            keep.command,
            Command::Session {
                command: SessionCommand::Keep { ref id }
            } if id == "session-child"
        ));

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

        let fork = Cli::try_parse_from([
            "dwo",
            "session",
            "prompt",
            "try another approach",
            "--from",
            "session-child",
        ])
        .unwrap();
        assert!(matches!(
            fork.command,
            Command::Session {
                command: SessionCommand::Prompt {
                    from: Some(ref id),
                    to: None,
                    ephemeral: false,
                    ..
                }
            } if id == "session-child"
        ));
        assert!(
            Cli::try_parse_from([
                "dwo",
                "session",
                "prompt",
                "invalid",
                "--from",
                "session-a",
                "--to",
                "session-b",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "dwo",
                "session",
                "prompt",
                "invalid",
                "--from",
                "session-a",
                "--ephemeral",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "dwo",
                "session",
                "prompt",
                "invalid",
                "--to",
                "session-a",
                "--ephemeral",
            ])
            .is_err()
        );

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
        assert!(Cli::try_parse_from(["dwo", "config-show"]).is_ok());
    }
}
