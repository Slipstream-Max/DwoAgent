use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use dwo_channel_api::{ChannelBundle, ChannelControl, ChannelFactory, ChannelGatewayApi};

use super::{ChannelGateway, ChannelManager};

/// Builds concrete channel managers and gateways for the host.
pub struct DefaultChannelFactory;

#[async_trait]
impl ChannelFactory for DefaultChannelFactory {
    async fn create(
        &self,
        profile_root: &Path,
        channels: &BTreeMap<String, serde_yaml::Value>,
    ) -> Result<ChannelBundle> {
        let manager = Arc::new(ChannelManager::new(profile_root, channels).await?);
        let gateway = Arc::new(ChannelGateway::new(manager.clone()));
        let control: Arc<dyn ChannelControl> = manager;
        let gateway: Arc<dyn ChannelGatewayApi> = gateway;
        Ok(ChannelBundle { control, gateway })
    }
}

/// Convenience constructor returning the shared default factory.
pub fn default_factory() -> Arc<dyn ChannelFactory> {
    Arc::new(DefaultChannelFactory)
}
