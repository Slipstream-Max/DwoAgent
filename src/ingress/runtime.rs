//! Lifecycle owner for long-lived service ingress channels.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::task::JoinHandle;

use super::bridge::{PendingConfirmationRegistry, SessionLeaseRegistry};
use super::config::{ChannelRuntimeConfig, load_channel_runtime_config};
use super::feishu::FeishuChannel;
use super::stdio::StdioChannel;
use super::websocket::WebSocketChannel;
use super::weixin::WeixinChannel;
use crate::agent::service::AgentService;
use crate::automation::AutomationNotificationSinks;

/// Start service ingress channels configured for one agent.
pub struct ChannelRuntime {
    agent: Arc<AgentService>,
    config: ChannelRuntimeConfig,
    lease_registry: Arc<SessionLeaseRegistry>,
    confirmation_registry: Arc<PendingConfirmationRegistry>,
    started: bool,
}

impl ChannelRuntime {
    pub fn new(agent: Arc<AgentService>, agent_structure_dir: &Path) -> Result<Self> {
        let config = load_channel_runtime_config(agent_structure_dir)?;
        Ok(Self {
            agent,
            config,
            lease_registry: Arc::new(SessionLeaseRegistry::new()),
            confirmation_registry: Arc::new(PendingConfirmationRegistry::new(agent_structure_dir)),
            started: false,
        })
    }

    pub fn config(&self) -> &ChannelRuntimeConfig {
        &self.config
    }

    pub fn lease_registry(&self) -> Arc<SessionLeaseRegistry> {
        self.lease_registry.clone()
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
        let result = self.run_inner(None).await;
        self.shutdown().await;
        result
    }

    pub async fn run_with_notification_sinks(
        &mut self,
        sinks_tx: tokio::sync::oneshot::Sender<AutomationNotificationSinks>,
    ) -> Result<()> {
        self.start().await?;
        let result = self.run_inner(Some(sinks_tx)).await;
        self.shutdown().await;
        result
    }

    async fn run_inner(
        &self,
        sinks_tx: Option<tokio::sync::oneshot::Sender<AutomationNotificationSinks>>,
    ) -> Result<()> {
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<Result<()>>(8);
        let mut tasks: Vec<JoinHandle<()>> = Vec::new();
        let mut weixin_clients = Vec::new();
        let mut notification_sinks = AutomationNotificationSinks::default();

        if self.config.stdio.enabled {
            let stdio = StdioChannel::new(
                self.agent.clone(),
                self.lease_registry.clone(),
                self.agent.agent_structure_dir(),
                &self.config.stdio,
            )?;
            let tx = result_tx.clone();
            tasks.push(tokio::spawn(async move {
                let _ = tx.send(stdio.run().await).await;
            }));
        }

        if self.config.websocket.enabled {
            let websocket = WebSocketChannel::new(
                self.agent.clone(),
                self.lease_registry.clone(),
                self.agent.agent_structure_dir(),
                &self.config.websocket,
            )?;
            let tx = result_tx.clone();
            tasks.push(tokio::spawn(async move {
                let _ = tx.send(websocket.run().await).await;
            }));
        }

        if self.config.weixin.enabled {
            let weixin = WeixinChannel::new(
                self.agent.clone(),
                self.lease_registry.clone(),
                self.confirmation_registry.clone(),
                self.agent.agent_structure_dir(),
                &self.config.weixin,
            )
            .await?;
            notification_sinks.weixin = Some(weixin.automation_notifier());
            let client = weixin.client();
            weixin_clients.push(client);
            let tx = result_tx.clone();
            tasks.push(tokio::spawn(async move {
                let _ = tx.send(weixin.run().await).await;
            }));
        }

        if self.config.feishu.enabled {
            let feishu = FeishuChannel::new(
                self.agent.clone(),
                self.lease_registry.clone(),
                self.confirmation_registry.clone(),
                self.agent.agent_structure_dir(),
                &self.config.feishu,
            )
            .await?;
            notification_sinks.feishu = Some(feishu.automation_notifier());
            let tx = result_tx.clone();
            tasks.push(tokio::spawn(async move {
                let _ = tx.send(feishu.run().await).await;
            }));
        }

        if let Some(sinks_tx) = sinks_tx {
            let _ = sinks_tx.send(notification_sinks);
        }

        drop(result_tx);

        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                for client in weixin_clients {
                    client.shutdown();
                }
                let shutdown_wait = tokio::time::sleep(Duration::from_secs(5));
                tokio::pin!(shutdown_wait);
                loop {
                    tokio::select! {
                        _ = &mut shutdown_wait => break,
                        result = result_rx.recv() => {
                            if result.is_none() {
                                break;
                            }
                        }
                    }
                }
                for task in tasks {
                    task.abort();
                }
                Ok(())
            }
            result = result_rx.recv() => {
                for task in tasks {
                    task.abort();
                }
                result.unwrap_or_else(|| Ok(()))
            }
        }
    }

    pub async fn shutdown(&mut self) {
        self.started = false;
    }

    fn validate_ready(&self) -> Result<()> {
        if !self.config.has_enabled_channels() {
            bail!("agent.yaml `channels` must enable at least one service ingress");
        }

        Ok(())
    }
}
