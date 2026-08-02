use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::future::join_all;
use tokio::sync::Mutex;

use crate::host::Host;

use super::ChannelKind;
use super::feishu::RunningFeishu;
use super::telegram::RunningTelegram;
use super::websocket::RunningWebsocket;
use super::weixin::RunningWeixin;

enum RunningChannel {
    Weixin(RunningWeixin),
    Telegram(RunningTelegram),
    Feishu(RunningFeishu),
    Websocket(RunningWebsocket),
}

impl RunningChannel {
    async fn start(channel: ChannelKind, host: Arc<Host>) -> Result<Self> {
        match channel {
            ChannelKind::Weixin => Ok(Self::Weixin(RunningWeixin::start(host).await?)),
            ChannelKind::Telegram => Ok(Self::Telegram(RunningTelegram::start(host).await?)),
            ChannelKind::Feishu => Ok(Self::Feishu(RunningFeishu::start(host).await?)),
            ChannelKind::Websocket => Ok(Self::Websocket(RunningWebsocket::start(host).await?)),
        }
    }

    async fn stop(self) {
        match self {
            Self::Weixin(channel) => channel.stop().await,
            Self::Telegram(channel) => channel.stop().await,
            Self::Feishu(channel) => channel.stop().await,
            Self::Websocket(channel) => channel.stop().await,
        }
    }

    async fn send_message(&self, text: &str) -> Result<()> {
        match self {
            Self::Weixin(channel) => channel.send_message(text).await,
            Self::Telegram(channel) => channel.send_message(text).await,
            Self::Feishu(channel) => channel.send_message(text).await,
            Self::Websocket(_) => anyhow::bail!("WebSocket channel does not support send-message"),
        }
    }

    async fn send_file(&self, path: &Path) -> Result<()> {
        match self {
            Self::Weixin(channel) => channel.send_file(path).await,
            Self::Telegram(channel) => channel.send_file(path).await,
            Self::Feishu(channel) => channel.send_file(path).await,
            Self::Websocket(_) => anyhow::bail!("WebSocket channel does not support send-file"),
        }
    }
}

pub struct ChannelHub {
    active: Mutex<HashMap<ChannelKind, Arc<Mutex<Option<RunningChannel>>>>>,
    operations: Mutex<HashMap<ChannelKind, Arc<Mutex<()>>>>,
}

impl ChannelHub {
    pub fn new() -> Self {
        Self {
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
        let running = RunningChannel::start(channel, host).await?;
        self.active
            .lock()
            .await
            .insert(channel, Arc::new(Mutex::new(Some(running))));
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
            && let Some(running) = slot.lock().await.take()
        {
            running.stop().await;
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
