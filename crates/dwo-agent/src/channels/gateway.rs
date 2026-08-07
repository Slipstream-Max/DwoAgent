use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::join_all;
use tokio::sync::Mutex;

use crate::host::Host;

use super::ChannelKind;
use super::bridge::{ChannelIngress, ConversationId, ConversationTransport, SessionBridge};
use super::feishu::FeishuAdapter;
use super::qq::QqAdapter;
use super::telegram::TelegramAdapter;
use super::websocket::RunningWebsocket;
use super::weixin::WeixinAdapter;

#[async_trait]
pub(crate) trait ChannelRuntime: Send + Sync {
    async fn stop(self: Box<Self>);
    async fn send_message(&self, text: &str) -> Result<()>;
    async fn send_file(&self, path: &Path) -> Result<()>;
}

#[async_trait]
pub(crate) trait ChannelStarter: Send {
    async fn start(
        self: Box<Self>,
        ingress: Arc<dyn ChannelIngress>,
    ) -> Result<Box<dyn ChannelRuntime>>;
}

#[async_trait]
pub(crate) trait ChannelAdapter: Send + Sync {
    async fn prepare(&self, host: Arc<Host>) -> Result<PreparedChannel>;
}

pub(crate) struct PreparedChannel {
    pub(crate) conversation: ConversationId,
    pub(crate) replay_turns: usize,
    pub(crate) selected_session_id: Option<String>,
    pub(crate) transport: Arc<dyn ConversationTransport>,
    pub(crate) starter: Box<dyn ChannelStarter>,
}

struct ActiveChannel {
    runtime: Box<dyn ChannelRuntime>,
    bridge: Option<Arc<SessionBridge>>,
}

pub struct ChannelGateway {
    adapters: HashMap<ChannelKind, Arc<dyn ChannelAdapter>>,
    active: Mutex<HashMap<ChannelKind, Arc<Mutex<Option<ActiveChannel>>>>>,
    operations: Mutex<HashMap<ChannelKind, Arc<Mutex<()>>>>,
}

impl ChannelGateway {
    pub fn new() -> Self {
        let adapters: HashMap<ChannelKind, Arc<dyn ChannelAdapter>> = HashMap::from([
            (
                ChannelKind::Weixin,
                Arc::new(WeixinAdapter) as Arc<dyn ChannelAdapter>,
            ),
            (
                ChannelKind::Telegram,
                Arc::new(TelegramAdapter) as Arc<dyn ChannelAdapter>,
            ),
            (
                ChannelKind::Feishu,
                Arc::new(FeishuAdapter) as Arc<dyn ChannelAdapter>,
            ),
            (
                ChannelKind::Qq,
                Arc::new(QqAdapter) as Arc<dyn ChannelAdapter>,
            ),
        ]);
        Self {
            adapters,
            active: Mutex::new(HashMap::new()),
            operations: Mutex::new(HashMap::new()),
        }
    }

    pub async fn start_all(self: &Arc<Self>, host: Arc<Host>) {
        let channels = match host.channels().list().await {
            Ok(channels) => channels,
            Err(error) => {
                tracing::error!(
                    event = "channel.load_failed",
                    error = %format!("{error:#}"),
                    "load channels failed"
                );
                return;
            }
        };
        for summary in channels
            .into_iter()
            .filter(|channel| channel.enabled && channel.connected)
        {
            let Some(channel) = ChannelKind::parse(&summary.name) else {
                continue;
            };
            if let Err(error) = self.start(channel, host.clone()).await {
                tracing::error!(
                    event = "channel.start_failed",
                    channel = channel.as_str(),
                    error = %format!("{error:#}"),
                    "start channel failed"
                );
            }
        }
    }

    pub async fn start(self: &Arc<Self>, channel: ChannelKind, host: Arc<Host>) -> Result<()> {
        let operation = self.operation(channel).await;
        let _operation = operation.lock().await;
        if self.active.lock().await.contains_key(&channel) {
            return Ok(());
        }
        let (runtime, bridge) = if channel == ChannelKind::Websocket {
            (
                Box::new(RunningWebsocket::start(host).await?) as Box<dyn ChannelRuntime>,
                None,
            )
        } else {
            let adapter = self.adapters.get(&channel).cloned().with_context(|| {
                format!("{} channel adapter is not registered", channel.as_str())
            })?;
            let prepared = adapter.prepare(host.clone()).await?;
            let bridge = Arc::new(SessionBridge::new(
                host,
                prepared.conversation,
                prepared.replay_turns,
                prepared.selected_session_id,
                prepared.transport,
            ));
            if let Err(error) = bridge.resume_observer().await {
                tracing::warn!(
                    event = "channel.observer_restore_failed",
                    channel = channel.as_str(),
                    error = %format!("{error:#}"),
                    "restore channel session observer failed"
                );
            }
            let ingress: Arc<dyn ChannelIngress> = bridge.clone();
            let runtime = prepared.starter.start(ingress).await?;
            (runtime, Some(bridge))
        };
        self.active.lock().await.insert(
            channel,
            Arc::new(Mutex::new(Some(ActiveChannel { runtime, bridge }))),
        );
        tracing::info!(
            event = "channel.started",
            channel = channel.as_str(),
            "channel started"
        );
        Ok(())
    }

    pub async fn stop(&self, channel: ChannelKind) {
        let operation = self.operation(channel).await;
        let _operation = operation.lock().await;
        let slot = self.active.lock().await.remove(&channel);
        if let Some(slot) = slot
            && let Some(active) = slot.lock().await.take()
        {
            active.runtime.stop().await;
            if let Some(bridge) = active.bridge {
                bridge.stop().await;
            }
            tracing::info!(
                event = "channel.stopped",
                channel = channel.as_str(),
                "channel stopped"
            );
        }
    }

    pub async fn stop_all(&self) {
        join_all(
            ChannelKind::ALL
                .into_iter()
                .map(|channel| self.stop(channel)),
        )
        .await;
    }

    pub async fn is_running(&self, channel: ChannelKind) -> bool {
        self.active.lock().await.contains_key(&channel)
    }

    pub async fn send_message(&self, channel: ChannelKind, text: &str) -> Result<()> {
        let slot = self
            .active
            .lock()
            .await
            .get(&channel)
            .cloned()
            .with_context(|| format!("{} channel is not running", channel.display_name()))?;
        let running = slot.lock().await;
        running
            .as_ref()
            .context("channel stopped before message delivery")?
            .runtime
            .send_message(text)
            .await
    }

    pub async fn send_file(&self, channel: ChannelKind, path: &Path) -> Result<()> {
        let slot = self
            .active
            .lock()
            .await
            .get(&channel)
            .cloned()
            .with_context(|| format!("{} channel is not running", channel.display_name()))?;
        let running = slot.lock().await;
        running
            .as_ref()
            .context("channel stopped before file delivery")?
            .runtime
            .send_file(path)
            .await
    }

    async fn operation(&self, channel: ChannelKind) -> Arc<Mutex<()>> {
        self.operations
            .lock()
            .await
            .entry(channel)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_adapters_are_registered_behind_one_gateway_interface() {
        let gateway = ChannelGateway::new();
        assert_eq!(gateway.adapters.len(), 4);
        for channel in [
            ChannelKind::Weixin,
            ChannelKind::Telegram,
            ChannelKind::Feishu,
            ChannelKind::Qq,
        ] {
            assert!(gateway.adapters.contains_key(&channel));
        }
        assert!(!gateway.adapters.contains_key(&ChannelKind::Websocket));
    }
}
