use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::host::Host;

use super::telegram::RunningTelegram;
use super::weixin::RunningWeixin;

pub struct ChannelHub {
    weixin: Mutex<Option<RunningWeixin>>,
    telegram: Mutex<Option<RunningTelegram>>,
}

impl ChannelHub {
    pub fn new() -> Self {
        Self {
            weixin: Mutex::new(None),
            telegram: Mutex::new(None),
        }
    }

    pub async fn start_all(self: &Arc<Self>, host: Arc<Host>) {
        let channels = match host.channels.list().await {
            Ok(channels) => channels,
            Err(error) => {
                eprintln!("load channels: {error:#}");
                return;
            }
        };
        let should_start = channels
            .iter()
            .any(|channel| channel.name == "weixin" && channel.enabled && channel.connected);
        if should_start && let Err(error) = self.start_weixin(host.clone()).await {
            eprintln!("start Weixin channel: {error:#}");
        }
        let should_start = channels
            .iter()
            .any(|channel| channel.name == "telegram" && channel.enabled && channel.connected);
        if should_start && let Err(error) = self.start_telegram(host).await {
            eprintln!("start Telegram channel: {error:#}");
        }
    }

    pub async fn start_weixin(self: &Arc<Self>, host: Arc<Host>) -> Result<()> {
        let mut active = self.weixin.lock().await;
        if active.is_none() {
            *active = Some(RunningWeixin::start(host).await?);
        }
        Ok(())
    }

    pub async fn start_telegram(self: &Arc<Self>, host: Arc<Host>) -> Result<()> {
        let mut active = self.telegram.lock().await;
        if active.is_none() {
            *active = Some(RunningTelegram::start(host).await?);
        }
        Ok(())
    }

    pub async fn stop_weixin(&self) {
        if let Some(active) = self.weixin.lock().await.take() {
            active.stop().await;
        }
    }

    pub async fn stop_telegram(&self) {
        if let Some(active) = self.telegram.lock().await.take() {
            active.stop().await;
        }
    }

    pub async fn stop_all(&self) {
        self.stop_weixin().await;
        self.stop_telegram().await;
    }

    pub async fn send_weixin_message(&self, to: &str, text: &str) -> Result<()> {
        let active = self.weixin.lock().await;
        active
            .as_ref()
            .context("Weixin channel is not running")?
            .send_message(to, text)
            .await
    }

    pub async fn send_weixin_file(&self, to: &str, path: &Path) -> Result<()> {
        let active = self.weixin.lock().await;
        active
            .as_ref()
            .context("Weixin channel is not running")?
            .send_file(to, path)
            .await
    }

    pub async fn send_telegram_message(&self, text: &str) -> Result<()> {
        let active = self.telegram.lock().await;
        active
            .as_ref()
            .context("Telegram channel is not running")?
            .send_message(text)
            .await
    }

    pub async fn send_telegram_file(&self, path: &Path) -> Result<()> {
        let active = self.telegram.lock().await;
        active
            .as_ref()
            .context("Telegram channel is not running")?
            .send_file(path)
            .await
    }
}
