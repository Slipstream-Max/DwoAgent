//! Lifecycle owner for long-lived service ingress channels.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::config::{ChannelRuntimeConfig, load_channel_runtime_config};
use super::weixin::WeixinChannel;
use crate::agent::service::AgentService;

/// Start service ingress channels configured for one agent.
pub struct ChannelRuntime {
    agent: Arc<AgentService>,
    config: ChannelRuntimeConfig,
    started: bool,
}

impl ChannelRuntime {
    pub fn new(agent: Arc<AgentService>, agent_structure_dir: &Path) -> Result<Self> {
        let config = load_channel_runtime_config(agent_structure_dir)?;
        Ok(Self {
            agent,
            config,
            started: false,
        })
    }

    pub fn config(&self) -> &ChannelRuntimeConfig {
        &self.config
    }

    pub async fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.validate_ready()?;
        self.started = true;
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        self.start().await?;
        let result = self.run_inner().await;
        self.shutdown().await;
        result
    }

    async fn run_inner(&self) -> Result<()> {
        if self.config.weixin.enabled {
            let weixin = WeixinChannel::new(
                self.agent.clone(),
                self.agent.agent_structure_dir(),
                &self.config.weixin,
            )
            .await?;
            let client = weixin.client();
            let mut task = tokio::spawn(async move { weixin.run().await });
            tokio::select! {
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    client.shutdown();
                    task.await?
                }
                result = &mut task => result?,
            }
        } else {
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
    }

    pub async fn shutdown(&mut self) {
        self.started = false;
    }

    fn validate_ready(&self) -> Result<()> {
        if !self.config.has_enabled_channels() {
            bail!("channels.yaml must enable at least one service ingress");
        }

        let mut enabled_optional: Vec<&str> = Vec::new();
        if self.config.websocket.enabled {
            enabled_optional.push("websocket");
        }
        if self.config.feishu.enabled {
            enabled_optional.push("feishu");
        }

        if !enabled_optional.is_empty() {
            bail!(
                "Service ingress channels are configured but not implemented yet: {}",
                enabled_optional.join(", ")
            );
        }
        Ok(())
    }
}
