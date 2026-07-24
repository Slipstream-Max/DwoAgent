use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::host::Host;

use super::weixin::RunningWeixin;

pub struct ChannelHub {
    weixin: Mutex<Option<RunningWeixin>>,
}

impl ChannelHub {
    pub fn new() -> Self {
        Self {
            weixin: Mutex::new(None),
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
            .into_iter()
            .any(|channel| channel.name == "weixin" && channel.enabled && channel.connected);
        if should_start && let Err(error) = self.start_weixin(host).await {
            eprintln!("start Weixin channel: {error:#}");
        }
    }

    pub async fn start_weixin(self: &Arc<Self>, host: Arc<Host>) -> Result<()> {
        let mut active = self.weixin.lock().await;
        if active.is_none() {
            *active = Some(RunningWeixin::start(host).await?);
        }
        Ok(())
    }

    pub async fn stop(&self) {
        if let Some(active) = self.weixin.lock().await.take() {
            active.stop().await;
        }
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
}
