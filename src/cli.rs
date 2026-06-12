//! CLI entry point.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::host::{HostMode, run_host_sync};
use crate::ingress::{
    run_feishu_login_sync, run_stdio_connect_sync, run_stdio_login_sync, run_websocket_login_sync,
    run_weixin_login_sync,
};
use crate::tui::run_tui;

#[derive(Debug, Parser)]
#[command(name = "dwo-agent", about = "Dwo Agent (赤铎) CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run or connect ACP stdio.
    Acp(AcpArgs),

    /// Run long-lived service ingress channels and automation scheduler.
    Serve(AgentFolderArgs),

    /// Open a local terminal dashboard for an agent folder.
    Tui(AgentFolderArgs),

    /// Manage external channels.
    #[command(subcommand)]
    Channel(ChannelCommand),
}

#[derive(Debug, Subcommand)]
enum ChannelCommand {
    /// Log in to a channel and persist its credentials.
    #[command(subcommand)]
    Login(ChannelLoginCommand),
}

#[derive(Debug, Subcommand)]
enum ChannelLoginCommand {
    /// Generate and persist an ACP stdio bridge bearer token.
    #[command(alias = "acp")]
    Stdio(AgentFolderArgs),

    /// Log in to Weixin by scanning a QR code.
    Weixin(AgentFolderArgs),

    /// Save Feishu app credentials for the channel.
    Feishu(FeishuLoginArgs),

    /// Generate and persist a WebSocket bearer token.
    Websocket(AgentFolderArgs),
}

#[derive(Debug, clap::Args)]
struct AcpArgs {
    #[command(subcommand)]
    command: AcpCommand,
}

#[derive(Debug, Subcommand)]
enum AcpCommand {
    /// Connect stdio ACP to a running `dwo-agent serve`.
    Connect(AcpConnectArgs),

    /// Run the legacy embedded stdio ACP host in this process.
    Embedded(AgentFolderArgs),
}

#[derive(Debug, clap::Args)]
struct AcpConnectArgs {
    /// Agent folder or workspace root containing `agent-structure/`.
    #[arg(long, default_value = ".")]
    agent_folder: PathBuf,

    /// Explicit IPC endpoint. Defaults to channel_secret/stdio/daemon.yaml.
    #[arg(long)]
    ipc: Option<String>,
}

#[derive(Debug, clap::Args)]
struct AgentFolderArgs {
    /// Agent folder or workspace root containing `agent-structure/`.
    #[arg(long, default_value = ".")]
    agent_folder: PathBuf,
}

#[derive(Debug, clap::Args)]
struct FeishuLoginArgs {
    /// Agent folder or workspace root containing `agent-structure/`.
    #[arg(long, default_value = ".")]
    agent_folder: PathBuf,

    /// Feishu app id. Falls back to FEISHU_APP_ID.
    #[arg(long)]
    app_id: Option<String>,

    /// Feishu app secret. Falls back to FEISHU_APP_SECRET.
    #[arg(long)]
    app_secret: Option<String>,
}

/// CLI entry point invoked by `main.rs`.
pub fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Acp(args) => match args.command {
            AcpCommand::Connect(connect) => {
                run_stdio_connect_sync(connect.agent_folder, connect.ipc)
            }
            AcpCommand::Embedded(embedded) => {
                run_host_sync(embedded.agent_folder, HostMode::AcpStdio)
            }
        },
        Command::Serve(args) => run_host_sync(args.agent_folder, HostMode::ServiceIngress),
        Command::Tui(args) => run_tui(args.agent_folder),
        Command::Channel(ChannelCommand::Login(ChannelLoginCommand::Stdio(args))) => {
            run_stdio_login_sync(args.agent_folder)
        }
        Command::Channel(ChannelCommand::Login(ChannelLoginCommand::Weixin(args))) => {
            run_weixin_login_sync(args.agent_folder)
        }
        Command::Channel(ChannelCommand::Login(ChannelLoginCommand::Feishu(args))) => {
            run_feishu_login_sync(args.agent_folder, args.app_id, args.app_secret)
        }
        Command::Channel(ChannelCommand::Login(ChannelLoginCommand::Websocket(args))) => {
            run_websocket_login_sync(args.agent_folder)
        }
    }
}
