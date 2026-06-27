//! Host process wiring for one AgentService instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::agent::service::AgentService;
use crate::ingress::run_rpc_stdio;

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
    let _ = agent_structure_dir;
    run_rpc_stdio(agent).await
}
