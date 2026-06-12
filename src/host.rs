//! Host process wiring for one AgentService instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::agent::service::AgentService;
use crate::automation::{AutomationNotificationSinks, AutomationRuntime, load_automation_config};
use crate::ingress::{ChannelRuntime, run_acp_stdio};

#[derive(Debug, Clone, Copy)]
pub enum HostMode {
    AcpStdio,
    ServiceIngress,
}

/// Synchronous entry point used by `main.rs`.
pub fn run_host_sync(agent_folder: PathBuf, mode: HostMode) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_host(&agent_folder, mode))
}

async fn run_host(agent_folder: &Path, mode: HostMode) -> Result<()> {
    let agent = Arc::new(AgentService::new(agent_folder)?);
    match mode {
        HostMode::AcpStdio => run_acp_stdio(agent).await,
        HostMode::ServiceIngress => {
            let agent_structure_dir = agent.agent_structure_dir().to_path_buf();
            run_service(agent, &agent_structure_dir).await
        }
    }
}

async fn run_service(agent: Arc<AgentService>, agent_structure_dir: &Path) -> Result<()> {
    let mut channels = ChannelRuntime::new(agent.clone(), agent_structure_dir)?;
    let automation_config = load_automation_config(agent_structure_dir)?;
    let has_channels = channels.config().has_enabled_channels();
    let has_automation = automation_config.has_enabled_jobs();

    if !has_channels && !has_automation {
        bail!("serve requires at least one enabled channel or automation job");
    }

    match (has_channels, has_automation) {
        (true, false) => channels.run().await,
        (false, true) => {
            let automation = AutomationRuntime::new(
                agent,
                channels.lease_registry(),
                agent_structure_dir,
                automation_config.jobs,
                AutomationNotificationSinks::default(),
            );
            run_automation_with_signal(automation).await
        }
        (true, true) => {
            run_channels_and_automation(
                agent,
                agent_structure_dir,
                channels,
                automation_config.jobs,
            )
            .await
        }
        (false, false) => unreachable!(),
    }
}

async fn run_channels_and_automation(
    agent: Arc<AgentService>,
    agent_structure_dir: &Path,
    mut channels: ChannelRuntime,
    jobs: Vec<crate::automation::AutomationJobConfig>,
) -> Result<()> {
    let leases = channels.lease_registry();
    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<Result<()>>(2);
    let (sinks_tx, sinks_rx) = tokio::sync::oneshot::channel();

    let tx = result_tx.clone();
    let channel_task = tokio::spawn(async move {
        let result = channels.run_with_notification_sinks(sinks_tx).await;
        let _ = tx.send(result).await;
    });

    let sinks = tokio::select! {
        maybe_sinks = sinks_rx => match maybe_sinks {
            Ok(sinks) => sinks,
            Err(_) => {
                channel_task.abort();
                bail!("channel runtime exited before automation notification sinks were ready");
            }
        },
        result = result_rx.recv() => {
            channel_task.abort();
            return result.unwrap_or_else(|| Ok(()));
        }
    };

    let automation = AutomationRuntime::new(agent, leases, agent_structure_dir, jobs, sinks);
    let tx = result_tx.clone();
    let automation_task = tokio::spawn(async move {
        let result = automation.run().await;
        let _ = tx.send(result).await;
    });
    drop(result_tx);

    let result = result_rx.recv().await.unwrap_or_else(|| Ok(()));
    channel_task.abort();
    automation_task.abort();
    result
}

async fn run_automation_with_signal(automation: AutomationRuntime) -> Result<()> {
    tokio::select! {
        result = automation.run() => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
