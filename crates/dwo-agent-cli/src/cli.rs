//! CLI entry point.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::create::{
    CreateAgentOptions, CreateSupervisorOptions, create_agent_profile, create_supervisor_config,
};
use crate::doctor::{DoctorOptions, run_doctor};
use dwo_agent_core::host::{HostMode, run_host_sync};
use dwo_agent_core::ingress::{run_feishu_login_sync, run_weixin_login_sync};
use dwo_agent_supervisor::{
    disable_supervisor, enable_supervisor, run_acp_shim_sync, run_supervisor_sync,
    start_supervisor, stop_supervisor, supervisor_status,
};

#[derive(Debug, Parser)]
#[command(name = "dwoagent", about = "Dwo Agent (赤铎) CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one agent profile directly.
    #[command(subcommand)]
    Agent(AgentCommand),

    /// Manage the machine-level supervisor.
    #[command(subcommand)]
    Supervisor(SupervisorCommand),

    /// Create agent or supervisor configuration.
    #[command(subcommand)]
    Create(CreateCommand),

    /// Check or prepare local Dwo Agent prerequisites.
    Doctor(DoctorArgs),

    /// Manage external channel credentials.
    #[command(subcommand)]
    Channel(ChannelCommand),
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Start configured external channels for one agent profile.
    Run(AgentProfileArgs),
}

#[derive(Debug, clap::Args)]
struct AgentProfileArgs {
    /// Agent profile path.
    #[arg(long)]
    agent_profile: PathBuf,
}

#[derive(Debug, Subcommand)]
enum SupervisorCommand {
    /// Register supervisor startup for user login.
    Enable,

    /// Start the supervisor in the background.
    Start,

    /// Stop running supervisor processes.
    Stop,

    /// Unregister supervisor startup for user login.
    Disable,

    /// Show supervisor startup and process status.
    Status,

    /// ACP stdio shim routed through supervisor.
    Acp(AgentProfileArgs),

    /// Run the supervisor in the foreground.
    #[command(hide = true)]
    Run(SupervisorRunArgs),
}

#[derive(Debug, clap::Args)]
struct SupervisorRunArgs {
    /// Supervisor config path.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum CreateCommand {
    /// Interactively create an agent profile.
    Agent(CreateAgentArgs),

    /// Create supervisor configuration.
    Supervisor(CreateSupervisorArgs),
}

#[derive(Debug, clap::Args)]
struct CreateAgentArgs {
    /// Agent profile name.
    #[arg(long)]
    name: String,

    /// Output profile path. Defaults to ~/.dwoagent/profiles/<name>.
    #[arg(long)]
    path: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct CreateSupervisorArgs {
    /// Generate a default supervisor config without prompts.
    #[arg(long)]
    default: bool,

    /// Output supervisor config path. Defaults to ~/.dwoagent/supervisor.yaml.
    #[arg(long)]
    path: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct DoctorArgs {
    /// Check local environment dependencies. This is the default.
    #[arg(long)]
    check: bool,

    /// Install missing local environment dependencies.
    #[arg(long)]
    resolve: bool,

    /// Run resolve actions without confirmation.
    #[arg(long, requires = "resolve")]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum ChannelCommand {
    /// Log in to a channel and persist its credentials.
    #[command(subcommand)]
    Login(ChannelLoginCommand),
}

#[derive(Debug, Subcommand)]
enum ChannelLoginCommand {
    /// Log in to Weixin by scanning a QR code.
    Weixin(AgentProfileArgs),

    /// Save Feishu app credentials for the channel.
    Feishu(FeishuLoginArgs),
}

#[derive(Debug, clap::Args)]
struct FeishuLoginArgs {
    /// Agent profile path.
    #[arg(long)]
    agent_profile: PathBuf,

    /// Feishu app id. Falls back to FEISHU_APP_ID.
    #[arg(long)]
    app_id: Option<String>,

    /// Feishu app secret. Falls back to FEISHU_APP_SECRET.
    #[arg(long)]
    app_secret: Option<String>,
}

impl From<DoctorArgs> for DoctorOptions {
    fn from(args: DoctorArgs) -> Self {
        Self {
            check: args.check,
            resolve: args.resolve,
            yes: args.yes,
        }
    }
}

/// CLI entry point invoked by `main.rs`.
pub fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Agent(AgentCommand::Run(args)) => {
            run_host_sync(args.agent_profile, HostMode::AgentRun)
        }
        Command::Supervisor(args) => match args {
            SupervisorCommand::Enable => {
                enable_supervisor()?;
                println!("Supervisor startup registered.");
                Ok(())
            }
            SupervisorCommand::Start => {
                if start_supervisor()? {
                    println!("Supervisor started.");
                } else {
                    println!("Supervisor is already running.");
                }
                Ok(())
            }
            SupervisorCommand::Stop => {
                let stopped = stop_supervisor()?;
                if stopped == 0 {
                    println!("Supervisor is not running.");
                } else {
                    println!("Stopped {stopped} supervisor process(es).");
                }
                Ok(())
            }
            SupervisorCommand::Disable => {
                if disable_supervisor()? {
                    println!("Supervisor startup unregistered.");
                } else {
                    println!("Supervisor startup was not registered.");
                }
                Ok(())
            }
            SupervisorCommand::Status => {
                println!("{}", supervisor_status()?);
                Ok(())
            }
            SupervisorCommand::Acp(args) => run_acp_shim_sync(args.agent_profile),
            SupervisorCommand::Run(args) => run_supervisor_sync(args.config),
        },
        Command::Create(CreateCommand::Agent(args)) => {
            create_agent_profile(CreateAgentOptions {
                name: args.name,
                path: args.path,
            })?;
            Ok(())
        }
        Command::Create(CreateCommand::Supervisor(args)) => {
            create_supervisor_config(CreateSupervisorOptions {
                default: args.default,
                path: args.path,
            })?;
            Ok(())
        }
        Command::Doctor(args) => run_doctor(args.into()),
        Command::Channel(ChannelCommand::Login(ChannelLoginCommand::Weixin(args))) => {
            run_weixin_login_sync(args.agent_profile)
        }
        Command::Channel(ChannelCommand::Login(ChannelLoginCommand::Feishu(args))) => {
            run_feishu_login_sync(args.agent_profile, args.app_id, args.app_secret)
        }
    }
}
