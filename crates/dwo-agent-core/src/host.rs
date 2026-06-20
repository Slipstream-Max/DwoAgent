//! Host process wiring for one AgentService instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::agent::service::AgentService;
use crate::automation::{load_automation_config, run_automation_jobs_with_leases};
use crate::ingress::{ChannelRuntime, SessionLeaseRegistry, run_rpc_stdio};

#[derive(Debug, Clone, Copy)]
pub enum HostMode {
    AgentRun,
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
        HostMode::AgentRun => {
            let agent_structure_dir = agent.agent_structure_dir().to_path_buf();
            run_profile_host(agent, &agent_structure_dir).await
        }
    }
}

async fn run_profile_host(agent: Arc<AgentService>, agent_structure_dir: &Path) -> Result<()> {
    let lease_registry = Arc::new(SessionLeaseRegistry::new());
    let channels = ChannelRuntime::new_with_leases(
        agent.clone(),
        agent_structure_dir,
        lease_registry.clone(),
    )?;
    let automation_config = load_automation_config(agent_structure_dir)?;
    let has_channels = channels.config().has_enabled_channels();
    let has_automation = automation_config.has_enabled_jobs();

    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<RuntimeResult>(3);
    let mut tasks = Vec::new();
    let external_task_count = usize::from(has_channels) + usize::from(has_automation);

    let tx = result_tx.clone();
    let rpc_agent = agent.clone();
    tasks.push(tokio::spawn(async move {
        let result = run_rpc_stdio(rpc_agent).await;
        let _ = tx
            .send(RuntimeResult {
                kind: RuntimeKind::Rpc,
                result,
            })
            .await;
    }));

    if has_channels {
        let tx = result_tx.clone();
        tasks.push(tokio::spawn(async move {
            let result = channels.run().await;
            let _ = tx
                .send(RuntimeResult {
                    kind: RuntimeKind::External,
                    result,
                })
                .await;
        }));
    }

    if has_automation {
        let agent_structure_dir = agent_structure_dir.to_path_buf();
        let jobs = automation_config.jobs;
        let tx = result_tx.clone();
        tasks.push(tokio::spawn(async move {
            let result =
                run_automation_jobs_with_leases(agent, &agent_structure_dir, jobs, lease_registry)
                    .await;
            let _ = tx
                .send(RuntimeResult {
                    kind: RuntimeKind::External,
                    result,
                })
                .await;
        }));
    }

    drop(result_tx);

    while let Some(runtime_result) = result_rx.recv().await {
        match runtime_result.kind {
            RuntimeKind::Rpc if external_task_count > 0 && runtime_result.result.is_ok() => {
                continue;
            }
            RuntimeKind::Rpc | RuntimeKind::External => {
                for task in tasks {
                    task.abort();
                }
                return runtime_result.result;
            }
        }
    }

    Ok(())
}

struct RuntimeResult {
    kind: RuntimeKind,
    result: Result<()>,
}

#[derive(Debug, Clone, Copy)]
enum RuntimeKind {
    Rpc,
    External,
}
