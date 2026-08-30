use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
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
    Section {
        #[command(subcommand)]
        command: SectionCommand,
    },
    Topic {
        #[command(subcommand)]
        command: TopicCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
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
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    Automation {
        #[arg(long, global = true)]
        project: Option<String>,
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
    Move {
        id: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        topic: String,
    },
    Set {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        policy: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        reasoning: Option<String>,
        #[arg(long)]
        worktree: Option<String>,
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
        #[arg(long, conflicts_with_all = ["cwd", "to", "from"])]
        project: Option<String>,
        #[arg(long, requires = "project")]
        topic: Option<String>,
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
enum SectionCommand {
    List {
        project: String,
    },
    Create {
        project: String,
        name: String,
    },
    Update {
        project: String,
        id: String,
        name: String,
    },
    Delete {
        project: String,
        id: String,
    },
    Reorder {
        project: String,
        id: String,
        position: usize,
    },
}

#[derive(Subcommand)]
enum TopicCommand {
    List {
        project: String,
    },
    Get {
        project: String,
        id: String,
    },
    Create {
        project: String,
        section: String,
        title: String,
    },
    Update {
        project: String,
        id: String,
        title: String,
    },
    Delete {
        project: String,
        id: String,
    },
    Move {
        project: String,
        id: String,
        section: String,
        #[arg(long)]
        to_project: Option<String>,
        #[arg(long, default_value_t = usize::MAX)]
        position: usize,
    },
    Reorder {
        project: String,
        id: String,
        section: String,
        position: usize,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    List,
    Get {
        project: String,
    },
    Create {
        name: String,
        #[arg(long, value_enum, default_value_t = ProjectKindArg::Shared)]
        kind: ProjectKindArg,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        from_session: Option<String>,
    },
    Update {
        project: String,
        name: String,
    },
    Repository {
        #[command(subcommand)]
        command: RepositoryCommand,
    },
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProjectKindArg {
    Shared,
    Independent,
}

impl ProjectKindArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Independent => "independent",
        }
    }
}

#[derive(Subcommand)]
enum RepositoryCommand {
    Get {
        project: String,
    },
    Clone {
        project: String,
        url: String,
        path: PathBuf,
        #[arg(long)]
        branch: Option<String>,
    },
    Attach {
        project: String,
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorktreeCommand {
    List {
        project: String,
    },
    Get {
        project: String,
        id: String,
    },
    Create {
        project: String,
        branch: String,
        path: PathBuf,
        #[arg(long)]
        start_point: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    Attach {
        project: String,
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    Rename {
        project: String,
        id: String,
        name: String,
    },
    Detach {
        project: String,
        id: String,
    },
    Remove {
        project: String,
        id: String,
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
    /// Show a configured MCP server with redacted configuration and runtime status.
    Get { name: String },
    /// Add an MCP server using Claude Code-compatible arguments.
    Add {
        /// MCP transport. DWO currently supports stdio and http.
        #[arg(short = 't', long, value_enum, default_value_t = McpTransportArg::Stdio)]
        transport: McpTransportArg,
        /// Environment variable for a stdio server, in KEY=value form. May be repeated.
        #[arg(short = 'e', long, value_name = "KEY=value")]
        env: Vec<String>,
        /// HTTP header for an HTTP server, in "Name: value" form. May be repeated.
        #[arg(short = 'H', long, value_name = "Name: value")]
        header: Vec<String>,
        /// MCP server name.
        name: String,
        /// HTTP URL for --transport http.
        #[arg(
            conflicts_with = "command",
            required_unless_present = "command",
            value_name = "URL"
        )]
        url: Option<String>,
        /// Stdio command and its arguments. Put these after `--`.
        #[arg(last = true, allow_hyphen_values = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Add one MCP server entry from JSON.
    AddJson { name: String, json: String },
    /// Remove an MCP server configuration.
    Remove { name: String },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum McpTransportArg {
    Stdio,
    #[value(alias = "streamable-http", alias = "streamableHttp")]
    Http,
}

#[derive(Subcommand)]
enum ModelCommand {
    /// List enabled models grouped by provider.
    List,
    /// Show the model and reasoning mode used for new sessions.
    GetDefault,
    /// Set the model and reasoning mode used for new sessions.
    SetDefault {
        /// Model reference in provider/model form.
        model: String,
        /// Reasoning mode supported by the selected model.
        #[arg(long)]
        reasoning: String,
        /// Context compaction trigger ratio used by models without an override.
        #[arg(long)]
        compaction_trigger_ratio: Option<f64>,
    },
}

#[derive(Subcommand)]
enum SkillsCommand {
    /// List installed skills.
    List,
    /// Add one Markdown file or a directory containing SKILL.md.
    Add {
        source: PathBuf,
        /// Destination skill name. Defaults to the source filename or directory name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove an installed skill.
    Remove { name: String },
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
        topic: Option<String>,
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
        Command::Section { command } => run_section(command, &config_path).await?,
        Command::Topic { command } => run_topic(command, &config_path).await?,
        Command::Project { command } => run_project(command, &config_path).await?,
        Command::ConfigShow => {
            let value = ipc::request_dwo(&config_path, "config.snapshot", json!({})).await?;
            render::write_value(&value)?;
        }
        Command::Channel { command } => run_channel(command, &config_path).await?,
        Command::Websocket { command } => run_websocket(command, &config_path).await?,
        Command::Mcp { command } => run_mcp(command, &config_path).await?,
        Command::Model { command } => run_model(command, &config_path).await?,
        Command::Skills { command } => run_skills(command, &config_path).await?,
        Command::Automation { project, command } => {
            run_automation(command, project, &config_path).await?
        }
        Command::Acp { protocol } => acp::run(config_path, protocol).await?,
    }
    Ok(())
}

async fn run_automation(
    command: AutomationCommand,
    project: Option<String>,
    config_path: &Path,
) -> Result<()> {
    anyhow::ensure!(
        !matches!(&command, AutomationCommand::Delete { .. }) || current_session_id().is_none(),
        "automation delete is unavailable inside an agent session"
    );
    let project_id = resolve_automation_project(config_path, project).await?;
    match command {
        AutomationCommand::List { json } => {
            let value = ipc::request_dwo(
                config_path,
                "automation.list",
                json!({"project_id": project_id}),
            )
            .await?;
            if json {
                render::write_value(&value)?;
            } else {
                let jobs: Vec<AutomationJobStatus> = serde_json::from_value(value)?;
                render::write_automation_list(&jobs)?;
            }
        }
        AutomationCommand::Status { job, json } => {
            let value = ipc::request_dwo(
                config_path,
                "automation.status",
                json!({"project_id": project_id, "job": job}),
            )
            .await?;
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
            topic,
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
                        title,
                    }
                }
                AutomationSessionArg::Fixed => {
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
                topic_id: topic,
                model: None,
                reasoning: None,
                policy: None,
            };
            let value = ipc::request_dwo(
                config_path,
                "automation.add",
                json!({"project_id": project_id, "job": job}),
            )
            .await?;
            if json {
                render::write_value(&value)?;
            } else {
                let status: AutomationJobStatus = serde_json::from_value(value)?;
                output::line(format_args!("Added automation {}", status.job.name))?;
            }
        }
        AutomationCommand::Enable { job, all } => {
            set_automation_enabled(config_path, &project_id, job, all, true).await?;
        }
        AutomationCommand::Disable { job, all } => {
            set_automation_enabled(config_path, &project_id, job, all, false).await?;
        }
        AutomationCommand::Delete { job, all, yes } => {
            anyhow::ensure!(!all || yes, "automation delete --all requires --yes");
            ipc::request_dwo(
                config_path,
                "automation.delete",
                json!({"project_id": project_id, "job": job, "all": all}),
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
                json!({
                    "project_id": project_id,
                    "job": job,
                    "caller_session_id": current_session_id()
                }),
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
    project_id: &str,
    job: Option<String>,
    all: bool,
    enabled: bool,
) -> Result<()> {
    let method = if enabled {
        "automation.enable"
    } else {
        "automation.disable"
    };
    ipc::request_dwo(
        config_path,
        method,
        json!({"project_id": project_id, "job": job, "all": all}),
    )
    .await?;
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

async fn resolve_automation_project(config_path: &Path, project: Option<String>) -> Result<String> {
    if let Some(project) = project.filter(|value| !value.trim().is_empty()) {
        return Ok(project);
    }
    let session_id = current_session_id().context(
        "--project is required when the command is not running inside a project session",
    )?;
    let value = ipc::request_dwo(config_path, "project.list", json!({})).await?;
    let projects: Vec<dwo_project::Project> = serde_json::from_value(value)?;
    projects
        .into_iter()
        .find(|project| {
            project.board.topics.iter().any(|topic| {
                topic
                    .session_ids
                    .iter()
                    .any(|assigned| assigned == &session_id)
            })
        })
        .map(|project| project.id)
        .with_context(|| format!("session {session_id} does not belong to a project"))
}

async fn run_mcp(command: McpCommand, config_path: &Path) -> Result<()> {
    match command {
        McpCommand::List => {
            let value = ipc::request_dwo(config_path, "mcp.list", json!({})).await?;
            let catalog: dwo_mcp::Catalog = serde_json::from_value(value)?;
            output::line(format_args!("{}", dwo_mcp::render_list(&catalog)))?;
        }
        McpCommand::Get { name } => {
            let value = ipc::request_dwo(config_path, "mcp.get", json!({"server": name})).await?;
            render::write_value(&value)?;
        }
        McpCommand::Add {
            transport,
            env,
            header,
            name,
            url,
            command,
        } => {
            let config = match transport {
                McpTransportArg::Stdio => {
                    anyhow::ensure!(
                        header.is_empty(),
                        "--header is only available with --transport http"
                    );
                    let env = parse_mcp_assignments(&env, '=', "--env")?;
                    anyhow::ensure!(url.is_none(), "a stdio command must be placed after --");
                    let (command, args) = command
                        .split_first()
                        .context("a stdio command is required after --")?;
                    json!({
                        "command": command,
                        "args": args,
                        "env": env,
                    })
                }
                McpTransportArg::Http => {
                    anyhow::ensure!(
                        env.is_empty(),
                        "--env is only available with --transport stdio"
                    );
                    anyhow::ensure!(
                        command.is_empty(),
                        "--transport http accepts a URL, not a command after --"
                    );
                    let headers = parse_mcp_assignments(&header, ':', "--header")?;
                    let url = url.context("--transport http requires a URL")?;
                    json!({
                        "type": "http",
                        "url": url,
                        "headers": headers,
                    })
                }
            };
            ipc::request_dwo(
                config_path,
                "mcp.install",
                json!({"server": name, "config": config}),
            )
            .await?;
            output::line(format_args!("Added MCP {name}"))?;
        }
        McpCommand::AddJson { name, json: source } => {
            let config: Value = serde_json::from_str(&source).context("parse MCP JSON")?;
            anyhow::ensure!(
                config.is_object(),
                "MCP JSON must be a server configuration object"
            );
            anyhow::ensure!(
                config.get("mcpServers").is_none(),
                "mcp add-json accepts one server entry, not an mcpServers wrapper"
            );
            ipc::request_dwo(
                config_path,
                "mcp.install",
                json!({"server": name, "config": config}),
            )
            .await?;
            output::line(format_args!("Added MCP {name}"))?;
        }
        McpCommand::Remove { name } => {
            let value =
                ipc::request_dwo(config_path, "mcp.uninstall", json!({"server": name})).await?;
            anyhow::ensure!(value["removed"] == true, "MCP server not found: {name}");
            output::line(format_args!("Removed MCP {name}"))?;
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

async fn run_model(command: ModelCommand, config_path: &Path) -> Result<()> {
    match command {
        ModelCommand::List => {
            let value = ipc::request_dwo(config_path, "model.available", json!({})).await?;
            render::write_model_list(&value)?;
        }
        ModelCommand::GetDefault => {
            let value = ipc::request_dwo(config_path, "model.get_default", json!({})).await?;
            render::write_value(&value)?;
        }
        ModelCommand::SetDefault {
            model,
            reasoning,
            compaction_trigger_ratio,
        } => {
            let value = ipc::request_dwo(
                config_path,
                "model.set_default",
                json!({
                    "model": model,
                    "reasoning": reasoning,
                    "compactionTriggerRatio": compaction_trigger_ratio,
                }),
            )
            .await?;
            render::write_value(&value)?;
        }
    }
    Ok(())
}

async fn run_skills(command: SkillsCommand, config_path: &Path) -> Result<()> {
    match command {
        SkillsCommand::List => {
            let value = ipc::request_dwo(config_path, "skill.list", json!({})).await?;
            render::write_value(&value)?;
        }
        SkillsCommand::Add { source, name } => {
            let import = read_skill_import(&source, name)?;
            let files = import
                .files
                .into_iter()
                .map(|file| {
                    json!({
                        "path": file.path,
                        "contentBase64": STANDARD.encode(file.content),
                    })
                })
                .collect::<Vec<_>>();
            let name = import.name;
            ipc::request_dwo(
                config_path,
                "skill.install",
                json!({"name": name, "files": files}),
            )
            .await?;
            output::line(format_args!("Added skill {name}"))?;
        }
        SkillsCommand::Remove { name } => {
            let value =
                ipc::request_dwo(config_path, "skill.uninstall", json!({"name": name})).await?;
            anyhow::ensure!(value["removed"] == true, "skill not found: {name}");
            output::line(format_args!("Removed skill {name}"))?;
        }
    }
    Ok(())
}

fn parse_mcp_assignments(
    values: &[String],
    separator: char,
    option: &str,
) -> Result<BTreeMap<String, String>> {
    let mut assignments = BTreeMap::new();
    for value in values {
        let (key, raw_value) = value
            .split_once(separator)
            .with_context(|| format!("{option} expects KEY{separator}VALUE"))?;
        let key = key.trim();
        anyhow::ensure!(!key.is_empty(), "{option} key must not be empty");
        let value = if separator == ':' {
            raw_value.trim_start().to_string()
        } else {
            raw_value.to_string()
        };
        anyhow::ensure!(
            assignments.insert(key.to_string(), value).is_none(),
            "{option} repeats key {key}"
        );
    }
    Ok(assignments)
}

struct SkillImport {
    name: String,
    files: Vec<SkillImportFile>,
}

struct SkillImportFile {
    path: String,
    content: Vec<u8>,
}

fn read_skill_import(source: &Path, name: Option<String>) -> Result<SkillImport> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("read skill source {}", source.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "skill source must not be a symbolic link: {}",
        source.display()
    );
    if metadata.is_file() {
        let extension = source.extension().and_then(|extension| extension.to_str());
        anyhow::ensure!(
            extension.is_some_and(|extension| extension.eq_ignore_ascii_case("md")),
            "skill file must have a .md extension: {}",
            source.display()
        );
        let inferred = skill_name_for_file(source)?;
        return Ok(SkillImport {
            name: name.unwrap_or(inferred),
            files: vec![SkillImportFile {
                path: "SKILL.md".to_string(),
                content: std::fs::read(source)
                    .with_context(|| format!("read skill file {}", source.display()))?,
            }],
        });
    }
    anyhow::ensure!(
        metadata.is_dir(),
        "skill source must be a Markdown file or directory: {}",
        source.display()
    );
    let inferred = source
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .context("skill directory has no usable name; pass --name")?
        .to_string();
    let mut files = Vec::new();
    collect_skill_import_files(source, source, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    anyhow::ensure!(
        files.iter().any(|file| file.path == "SKILL.md"),
        "skill directory must contain SKILL.md at its root"
    );
    Ok(SkillImport {
        name: name.unwrap_or(inferred),
        files,
    })
}

fn skill_name_for_file(source: &Path) -> Result<String> {
    let filename = source.file_name().and_then(|name| name.to_str());
    let name = if filename.is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md")) {
        source
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
    } else {
        source.file_stem().and_then(|name| name.to_str())
    };
    name.filter(|name| !name.is_empty())
        .map(str::to_string)
        .context("skill file has no usable name; pass --name")
}

fn collect_skill_import_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<SkillImportFile>,
) -> Result<()> {
    let mut entries = std::fs::read_dir(directory)
        .with_context(|| format!("read skill directory {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        anyhow::ensure!(
            !file_type.is_symlink(),
            "skill directory must not contain symbolic links: {}",
            path.display()
        );
        if file_type.is_dir() {
            collect_skill_import_files(root, &path, files)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("skill import path must remain below its root");
            let path = relative
                .components()
                .map(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .context("skill source path is not valid UTF-8")
                })
                .collect::<Result<Vec<_>>>()?
                .join("/");
            files.push(SkillImportFile {
                path,
                content: std::fs::read(entry.path())
                    .with_context(|| format!("read skill file {}", entry.path().display()))?,
            });
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
                "session.list",
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
        SessionCommand::Move { id, project, topic } => {
            let value = ipc::request_dwo(
                config_path,
                "project.topic.session.assign",
                json!({
                    "project_id": project,
                    "topic_id": topic,
                    "session_id": id,
                    "caller_session_id": current_session_id(),
                }),
            )
            .await?;
            render::write_value(&value)?;
        }
        SessionCommand::Set {
            id,
            title,
            policy,
            model,
            reasoning,
            worktree,
        } => {
            anyhow::ensure!(
                title.is_some()
                    || policy.is_some()
                    || model.is_some()
                    || reasoning.is_some()
                    || worktree.is_some(),
                "session set requires at least one field"
            );
            let policy = policy
                .map(|value| dwo_tools::SessionMode::parse(&value).map_err(anyhow::Error::msg))
                .transpose()?;
            let value = ipc::request_dwo(
                config_path,
                "session.set",
                json!({
                    "session_id": id,
                    "caller_session_id": current_session_id(),
                    "title": title,
                    "policy": policy,
                    "model": model,
                    "reasoning": reasoning,
                    "worktree_id": worktree,
                }),
            )
            .await?;
            render::write_value(&value)?;
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
            project,
            topic,
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
                    "project_id": project,
                    "topic_id": topic,
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

async fn run_section(command: SectionCommand, config_path: &Path) -> Result<()> {
    let (method, params) = match command {
        SectionCommand::List { project } => {
            let value =
                ipc::request_dwo(config_path, "project.board", json!({"project_id": project}))
                    .await?;
            let sections = value
                .get("board")
                .and_then(|board| board.get("sections"))
                .cloned()
                .context("project board response is missing sections")?;
            return render::write_value(&sections);
        }
        SectionCommand::Create { project, name } => (
            "project.section.create",
            json!({"project_id": project, "name": name}),
        ),
        SectionCommand::Update { project, id, name } => (
            "project.section.update",
            json!({"project_id": project, "section_id": id, "name": name}),
        ),
        SectionCommand::Delete { project, id } => (
            "project.section.delete",
            json!({"project_id": project, "section_id": id}),
        ),
        SectionCommand::Reorder {
            project,
            id,
            position,
        } => (
            "project.section.reorder",
            json!({"project_id": project, "section_id": id, "position": position}),
        ),
    };
    let value = ipc::request_dwo(config_path, method, params).await?;
    render::write_value(&value)
}

async fn run_topic(command: TopicCommand, config_path: &Path) -> Result<()> {
    let (method, params) = match command {
        TopicCommand::List { project } => {
            let value =
                ipc::request_dwo(config_path, "project.board", json!({"project_id": project}))
                    .await?;
            let topics = value
                .get("board")
                .and_then(|board| board.get("topics"))
                .cloned()
                .context("project board response is missing topics")?;
            return render::write_value(&topics);
        }
        TopicCommand::Get { project, id } => (
            "project.topic.get",
            json!({"project_id": project, "topic_id": id}),
        ),
        TopicCommand::Create {
            project,
            section,
            title,
        } => (
            "project.topic.create",
            json!({"project_id": project, "section_id": section, "title": title}),
        ),
        TopicCommand::Update { project, id, title } => (
            "project.topic.update",
            json!({"project_id": project, "topic_id": id, "title": title}),
        ),
        TopicCommand::Delete { project, id } => (
            "project.topic.delete",
            json!({"project_id": project, "topic_id": id}),
        ),
        TopicCommand::Move {
            project,
            id,
            section,
            to_project,
            position,
        } => {
            let target_project = to_project.unwrap_or_else(|| project.clone());
            if target_project == project {
                (
                    "project.topic.move",
                    json!({
                        "project_id": project,
                        "topic_id": id,
                        "section_id": section,
                        "position": position,
                    }),
                )
            } else {
                (
                    "project.topic.move_to_project",
                    json!({
                        "source_project_id": project,
                        "topic_id": id,
                        "target_project_id": target_project,
                        "target_section_id": section,
                        "position": position,
                    }),
                )
            }
        }
        TopicCommand::Reorder {
            project,
            id,
            section,
            position,
        } => (
            "project.topic.reorder",
            json!({
                "project_id": project,
                "topic_id": id,
                "section_id": section,
                "position": position,
            }),
        ),
    };
    let value = ipc::request_dwo(config_path, method, params).await?;
    render::write_value(&value)
}

async fn run_project(command: ProjectCommand, config_path: &Path) -> Result<()> {
    let (method, params) = match command {
        ProjectCommand::List => ("project.list", json!({})),
        ProjectCommand::Get { project } => ("project.get", json!({"project_id": project})),
        ProjectCommand::Create {
            name,
            kind,
            mut cwd,
            from_session,
        } => {
            if kind == ProjectKindArg::Independent {
                anyhow::ensure!(cwd.is_none(), "independent projects cannot define --cwd");
            } else if cwd.is_none() && from_session.is_none() {
                cwd = Some(std::env::current_dir()?);
            }
            (
                "project.create",
                json!({
                    "name": name,
                    "kind": kind.as_str(),
                    "pwd": cwd,
                    "from_session_id": from_session,
                    "caller_session_id": current_session_id(),
                }),
            )
        }
        ProjectCommand::Update { project, name } => (
            "project.update",
            json!({"project_id": project, "name": name}),
        ),
        ProjectCommand::Repository { command } => match command {
            RepositoryCommand::Get { project } => {
                ("project.repository.get", json!({"project_id": project}))
            }
            RepositoryCommand::Clone {
                project,
                url,
                path,
                branch,
            } => (
                "project.repository.clone",
                json!({"project_id": project, "url": url, "path": path, "branch": branch}),
            ),
            RepositoryCommand::Attach {
                project,
                path,
                name,
            } => (
                "project.repository.attach",
                json!({"project_id": project, "path": path, "name": name}),
            ),
        },
        ProjectCommand::Worktree { command } => match command {
            WorktreeCommand::List { project } => {
                ("project.worktree.list", json!({"project_id": project}))
            }
            WorktreeCommand::Get { project, id } => (
                "project.worktree.get",
                json!({"project_id": project, "worktree_id": id}),
            ),
            WorktreeCommand::Create {
                project,
                branch,
                path,
                start_point,
                name,
            } => (
                "project.worktree.create",
                json!({"project_id": project, "branch": branch, "path": path, "start_point": start_point, "name": name}),
            ),
            WorktreeCommand::Attach {
                project,
                path,
                name,
            } => (
                "project.worktree.attach",
                json!({"project_id": project, "path": path, "name": name}),
            ),
            WorktreeCommand::Rename { project, id, name } => (
                "project.worktree.update",
                json!({"project_id": project, "worktree_id": id, "name": name}),
            ),
            WorktreeCommand::Detach { project, id } => (
                "project.worktree.detach",
                json!({"project_id": project, "worktree_id": id}),
            ),
            WorktreeCommand::Remove { project, id } => (
                "project.worktree.remove",
                json!({"project_id": project, "worktree_id": id}),
            ),
        },
    };
    let value = ipc::request_dwo(config_path, method, params).await?;
    render::write_value(&value)
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
    fn parses_mcp_management_commands() {
        let stdio = Cli::try_parse_from([
            "dwo",
            "mcp",
            "add",
            "-e",
            "TOKEN=value",
            "filesystem",
            "--",
            "npx",
            "-y",
            "@modelcontextprotocol/server-filesystem",
        ])
        .unwrap();
        let Command::Mcp {
            command:
                McpCommand::Add {
                    transport,
                    env,
                    header,
                    name,
                    url,
                    command,
                },
        } = stdio.command
        else {
            panic!("expected stdio MCP add command")
        };
        assert_eq!(transport, McpTransportArg::Stdio);
        assert_eq!(env, vec!["TOKEN=value"]);
        assert!(header.is_empty());
        assert_eq!(name, "filesystem");
        assert_eq!(
            command,
            vec!["npx", "-y", "@modelcontextprotocol/server-filesystem"]
        );
        assert!(url.is_none());

        let http = Cli::try_parse_from([
            "dwo",
            "mcp",
            "add",
            "-t",
            "http",
            "-H",
            "Authorization: Bearer token",
            "notion",
            "https://mcp.notion.com/mcp",
        ])
        .unwrap();
        let Command::Mcp {
            command:
                McpCommand::Add {
                    transport,
                    header,
                    name,
                    url,
                    command,
                    ..
                },
        } = http.command
        else {
            panic!("expected HTTP MCP add command")
        };
        assert_eq!(transport, McpTransportArg::Http);
        assert_eq!(header, vec!["Authorization: Bearer token"]);
        assert_eq!(name, "notion");
        assert_eq!(url.as_deref(), Some("https://mcp.notion.com/mcp"));
        assert!(command.is_empty());

        let get = Cli::try_parse_from(["dwo", "mcp", "get", "notion"]).unwrap();
        assert!(matches!(
            get.command,
            Command::Mcp {
                command: McpCommand::Get { ref name }
            } if name == "notion"
        ));

        let add_json = Cli::try_parse_from([
            "dwo",
            "mcp",
            "add-json",
            "notion",
            r#"{"type":"http","url":"https://mcp.notion.com/mcp"}"#,
        ])
        .unwrap();
        assert!(matches!(
            add_json.command,
            Command::Mcp {
                command: McpCommand::AddJson { ref name, ref json }
            } if name == "notion" && json.contains("mcp.notion.com")
        ));

        let remove = Cli::try_parse_from(["dwo", "mcp", "remove", "notion"]).unwrap();
        assert!(matches!(
            remove.command,
            Command::Mcp {
                command: McpCommand::Remove { ref name }
            } if name == "notion"
        ));
    }

    #[test]
    fn parses_model_and_skill_management_commands() {
        let list = Cli::try_parse_from(["dwo", "model", "list"]).unwrap();
        assert!(matches!(
            list.command,
            Command::Model {
                command: ModelCommand::List
            }
        ));

        let get_default = Cli::try_parse_from(["dwo", "model", "get-default"]).unwrap();
        assert!(matches!(
            get_default.command,
            Command::Model {
                command: ModelCommand::GetDefault
            }
        ));

        let set_default = Cli::try_parse_from([
            "dwo",
            "model",
            "set-default",
            "deepseek/deepseek-v4-pro",
            "--reasoning",
            "high",
            "--compaction-trigger-ratio",
            "0.75",
        ])
        .unwrap();
        assert!(matches!(
            set_default.command,
            Command::Model {
                command: ModelCommand::SetDefault {
                    ref model,
                    ref reasoning,
                    compaction_trigger_ratio: Some(ratio),
                }
            } if model == "deepseek/deepseek-v4-pro" && reasoning == "high" && ratio == 0.75
        ));
        assert!(
            Cli::try_parse_from(["dwo", "model", "set-default", "deepseek/deepseek-v4-pro",])
                .is_err()
        );

        let add = Cli::try_parse_from([
            "dwo",
            "skills",
            "add",
            "C:/skills/release-notes",
            "--name",
            "release-notes",
        ])
        .unwrap();
        assert!(matches!(
            add.command,
            Command::Skills {
                command: SkillsCommand::Add { ref source, name: Some(ref name) }
            } if source == &PathBuf::from("C:/skills/release-notes") && name == "release-notes"
        ));

        let remove = Cli::try_parse_from(["dwo", "skills", "remove", "release-notes"]).unwrap();
        assert!(matches!(
            remove.command,
            Command::Skills {
                command: SkillsCommand::Remove { ref name }
            } if name == "release-notes"
        ));
    }

    #[test]
    fn reads_single_file_and_directory_skill_sources() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("release-notes.md");
        std::fs::write(&file, "Write release notes.").unwrap();
        let file_import = read_skill_import(&file, None).unwrap();
        assert_eq!(file_import.name, "release-notes");
        assert_eq!(file_import.files.len(), 1);
        assert_eq!(file_import.files[0].path, "SKILL.md");

        let directory = root.path().join("deploy");
        std::fs::create_dir_all(directory.join("references")).unwrap();
        std::fs::write(directory.join("SKILL.md"), "Deploy safely.").unwrap();
        std::fs::write(directory.join("references/example.txt"), "example").unwrap();
        let directory_import = read_skill_import(&directory, None).unwrap();
        assert_eq!(directory_import.name, "deploy");
        assert_eq!(
            directory_import
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["SKILL.md", "references/example.txt"]
        );
    }

    #[test]
    fn parses_automation_commands() {
        let run = Cli::try_parse_from(["dwo", "automation", "run", "daily-report"]).unwrap();
        assert!(matches!(
            run.command,
            Command::Automation {
                command: AutomationCommand::Run { ref job, json: false },
                ..
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
                },
                ..
            } if name == "daily-report"
        ));

        let enable_all = Cli::try_parse_from(["dwo", "automation", "enable", "--all"]).unwrap();
        assert!(matches!(
            enable_all.command,
            Command::Automation {
                command: AutomationCommand::Enable {
                    job: None,
                    all: true
                },
                ..
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

        let set = Cli::try_parse_from([
            "dwo",
            "session",
            "set",
            "session-child",
            "--title",
            "renamed",
            "--policy",
            "watch",
            "--model",
            "deepseek/deepseek-v4-flash",
            "--reasoning",
            "low",
        ])
        .unwrap();
        assert!(matches!(
            set.command,
            Command::Session {
                command: SessionCommand::Set {
                    ref id,
                    ref title,
                    ref policy,
                    ref model,
                    ref reasoning,
                    ..
                }
            } if id == "session-child"
                && title.as_deref() == Some("renamed")
                && policy.as_deref() == Some("watch")
                && model.as_deref() == Some("deepseek/deepseek-v4-flash")
                && reasoning.as_deref() == Some("low")
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

        let project_prompt = Cli::try_parse_from([
            "dwo",
            "session",
            "prompt",
            "work here",
            "--project",
            "project-1",
            "--topic",
            "topic-1",
        ])
        .unwrap();
        assert!(matches!(
            project_prompt.command,
            Command::Session {
                command: SessionCommand::Prompt {
                    project: Some(ref project),
                    topic: Some(ref topic),
                    cwd: None,
                    ..
                }
            } if project == "project-1" && topic == "topic-1"
        ));
        assert!(
            Cli::try_parse_from([
                "dwo",
                "session",
                "prompt",
                "invalid",
                "--project",
                "project-1",
                "--cwd",
                ".",
            ])
            .is_err()
        );

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

    #[test]
    fn parses_project_board_commands() {
        let project = Cli::try_parse_from(["dwo", "project", "create", "DwoAgent"]).unwrap();
        assert!(matches!(
            project.command,
            Command::Project {
                command: ProjectCommand::Create {
                    ref name,
                    kind: ProjectKindArg::Shared,
                    cwd: None,
                    from_session: None,
                }
            } if name == "DwoAgent"
        ));

        let section =
            Cli::try_parse_from(["dwo", "section", "create", "project-1", "Planning"]).unwrap();
        assert!(matches!(
            section.command,
            Command::Section {
                command: SectionCommand::Create { ref project, ref name }
            } if project == "project-1" && name == "Planning"
        ));

        let topic = Cli::try_parse_from([
            "dwo",
            "topic",
            "move",
            "project-1",
            "topic-1",
            "section-2",
            "--to-project",
            "project-2",
        ])
        .unwrap();
        assert!(matches!(
            topic.command,
            Command::Topic {
                command: TopicCommand::Move {
                    ref project,
                    ref id,
                    ref section,
                    ref to_project,
                    position,
                }
            } if project == "project-1"
                && id == "topic-1"
                && section == "section-2"
                && to_project.as_deref() == Some("project-2")
                && position == usize::MAX
        ));

        let session = Cli::try_parse_from([
            "dwo",
            "session",
            "move",
            "session-1",
            "--project",
            "project-2",
            "--topic",
            "topic-2",
        ])
        .unwrap();
        assert!(matches!(
            session.command,
            Command::Session {
                command: SessionCommand::Move {
                    ref id,
                    ref project,
                    ref topic,
                }
            } if id == "session-1" && project == "project-2" && topic == "topic-2"
        ));
    }
}
