use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::Value;
use tokio::sync::Mutex;

use super::bridge::{ChannelIngress, ConversationId, ConversationTransport, SessionBridge};
use super::feishu::FeishuAdapter;
use super::qq::QqAdapter;
use super::telegram::TelegramAdapter;
use super::weixin::WeixinAdapter;
use super::{
    ChannelBindingProgress, ChannelHost, ChannelKind, ChannelManager, ChannelPollParams,
};

#[async_trait]
pub trait ChannelRuntime: Send + Sync {
    async fn stop(self: Box<Self>);
    async fn send_message(&self, text: &str) -> Result<()>;
    async fn send_file(&self, path: &Path) -> Result<()>;
}

#[async_trait]
pub trait ChannelStarter: Send {
    async fn start(
        self: Box<Self>,
        ingress: Arc<dyn ChannelIngress>,
    ) -> Result<Box<dyn ChannelRuntime>>;
}

#[async_trait]
pub trait ChannelAdapter: ChannelBinder + Send + Sync {
    async fn prepare(
        &self,
        host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
    ) -> Result<PreparedChannel>;
}

#[async_trait]
pub trait ChannelBinder: Send + Sync {
    async fn begin_bind(
        &self,
        host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
    ) -> Result<Value>;
    async fn poll_bind(
        &self,
        host: Arc<dyn ChannelHost>,
        channels: Arc<ChannelManager>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress>;
}

pub struct PreparedChannel {
    pub conversation: ConversationId,
    pub replay_turns: usize,
    pub selected_session_id: Option<String>,
    pub transport: Arc<dyn ConversationTransport>,
    pub starter: Box<dyn ChannelStarter>,
}

struct ActiveChannel {
    runtime: Box<dyn ChannelRuntime>,
    bridge: Option<Arc<SessionBridge>>,
}

pub struct ChannelGateway {
    channels: Arc<ChannelManager>,
    adapters: HashMap<ChannelKind, Arc<dyn ChannelAdapter>>,
    active: Mutex<HashMap<ChannelKind, Arc<Mutex<Option<ActiveChannel>>>>>,
    operations: Mutex<HashMap<ChannelKind, Arc<Mutex<()>>>>,
}

impl ChannelGateway {
    pub fn new(channels: Arc<ChannelManager>) -> Self {
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
            channels,
            adapters,
            active: Mutex::new(HashMap::new()),
            operations: Mutex::new(HashMap::new()),
        }
    }

    pub async fn begin_bind(
        &self,
        channel: ChannelKind,
        host: Arc<dyn ChannelHost>,
    ) -> Result<Value> {
        self.stop(channel).await;
        let adapter =
            self.adapters.get(&channel).cloned().with_context(|| {
                format!("{} channel adapter is not registered", channel.as_str())
            })?;
        adapter.begin_bind(host, self.channels.clone()).await
    }

    pub async fn poll_bind(
        &self,
        channel: ChannelKind,
        host: Arc<dyn ChannelHost>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress> {
        let adapter =
            self.adapters.get(&channel).cloned().with_context(|| {
                format!("{} channel adapter is not registered", channel.as_str())
            })?;
        let progress = adapter
            .poll_bind(host.clone(), self.channels.clone(), params)
            .await?;
        if let ChannelBindingProgress::Confirmed { channel: summary } = &progress
            && summary.enabled
        {
            self.start(channel, host).await?;
        }
        Ok(progress)
    }

    pub async fn unbind(
        &self,
        channel: ChannelKind,
        _host: Arc<dyn ChannelHost>,
    ) -> Result<bool> {
        self.stop(channel).await;
        self.channels.remove(channel).await
    }

    pub async fn start_all(&self, host: Arc<dyn ChannelHost>) {
        let channels = match self.channels.list().await {
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

    pub async fn start(&self, channel: ChannelKind, host: Arc<dyn ChannelHost>) -> Result<()> {
        let operation = self.operation(channel).await;
        let _operation = operation.lock().await;
        if self.active.lock().await.contains_key(&channel) {
            return Ok(());
        }
        let adapter =
            self.adapters.get(&channel).cloned().with_context(|| {
                format!("{} channel adapter is not registered", channel.as_str())
            })?;
        let prepared = adapter
            .prepare(host.clone(), self.channels.clone())
            .await?;
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
        let bridge = Some(bridge);
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

#[async_trait]
impl dwo_channel_api::ChannelGatewayApi for ChannelGateway {
    async fn begin_bind(&self, channel: ChannelKind, host: Arc<dyn ChannelHost>) -> Result<Value> {
        ChannelGateway::begin_bind(self, channel, host).await
    }

    async fn poll_bind(
        &self,
        channel: ChannelKind,
        host: Arc<dyn ChannelHost>,
        params: ChannelPollParams,
    ) -> Result<ChannelBindingProgress> {
        ChannelGateway::poll_bind(self, channel, host, params).await
    }

    async fn unbind(&self, channel: ChannelKind, host: Arc<dyn ChannelHost>) -> Result<bool> {
        ChannelGateway::unbind(self, channel, host).await
    }

    async fn start_all(&self, host: Arc<dyn ChannelHost>) {
        ChannelGateway::start_all(self, host).await
    }

    async fn stop_all(&self) {
        ChannelGateway::stop_all(self).await
    }

    async fn send_message(&self, channel: ChannelKind, text: &str) -> Result<()> {
        ChannelGateway::send_message(self, channel, text).await
    }

    async fn send_file(&self, channel: ChannelKind, path: &Path) -> Result<()> {
        ChannelGateway::send_file(self, channel, path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn message_adapters_are_registered_behind_one_gateway_interface() {
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &std::collections::BTreeMap::new())
            .await
            .unwrap();
        let gateway = ChannelGateway::new(Arc::new(manager));
        assert_eq!(gateway.adapters.len(), 4);
        for channel in [
            ChannelKind::Weixin,
            ChannelKind::Telegram,
            ChannelKind::Feishu,
            ChannelKind::Qq,
        ] {
            assert!(gateway.adapters.contains_key(&channel));
        }
    }
}
