use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::Host;
use dwo_channels::{ChannelKind, ChannelPollParams};

#[derive(Clone, Copy)]
pub(crate) enum ManagedChannelAction {
    Status,
    Enable,
    Disable,
    Config,
    SendMessage,
    SendFile,
    Remove,
}

pub(crate) fn managed_channel_action(method: &str) -> Option<(ChannelKind, ManagedChannelAction)> {
    let (channel, action) = method.strip_prefix("channel.")?.split_once('.')?;
    let channel = ChannelKind::parse(channel)?;
    let action = match action {
        "status" => ManagedChannelAction::Status,
        "enable" => ManagedChannelAction::Enable,
        "disable" => ManagedChannelAction::Disable,
        "config" => ManagedChannelAction::Config,
        "send_message" => ManagedChannelAction::SendMessage,
        "send_file" => ManagedChannelAction::SendFile,
        "remove" => ManagedChannelAction::Remove,
        _ => return None,
    };
    Some((channel, action))
}

impl Host {
    pub(crate) async fn channel_list(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.channels().list().await?)?)
    }

    pub(crate) async fn channel_begin_bind(
        self: &Arc<Self>,
        channel: ChannelKind,
    ) -> Result<Value> {
        let result = self
            .channel_gateway
            .begin_bind(channel, self.clone())
            .await?;
        self.events
            .publish(
                "channel.status",
                json!({"channel": channel.as_str(), "status": "binding"}),
            )
            .await;
        Ok(result)
    }

    pub(crate) async fn channel_poll_bind(
        self: &Arc<Self>,
        channel: ChannelKind,
        params: ChannelPollParams,
    ) -> Result<Value> {
        Ok(serde_json::to_value(
            self.channel_gateway
                .poll_bind(channel, self.clone(), params)
                .await?,
        )?)
    }

    pub(crate) async fn channel_unbind(self: &Arc<Self>, channel: ChannelKind) -> Result<Value> {
        let unbound = self.channel_gateway.unbind(channel, self.clone()).await?;
        self.events
            .publish(
                "channel.status",
                json!({"channel": channel.as_str(), "status": "unbound"}),
            )
            .await;
        Ok(json!({"unbound": unbound}))
    }

    pub(crate) async fn channel_status(&self, channel: ChannelKind) -> Result<Value> {
        let value = serde_json::to_value(self.channels().summary(channel).await?)?;
        Ok(value)
    }

    pub(crate) async fn channel_set_enabled(
        self: &Arc<Self>,
        channel: ChannelKind,
        enabled: bool,
    ) -> Result<Value> {
        let name = channel.as_str().to_string();
        self.config_manager
            .update(|profile| {
                let entry = profile
                    .channels
                    .get_mut(&name)
                    .context("channel is not configured")?;
                let object = entry
                    .as_mapping_mut()
                    .context("channel config must be an object")?;
                object.insert(
                    serde_yaml::Value::String("enabled".to_string()),
                    serde_yaml::Value::Bool(enabled),
                );
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        self.events
            .publish(
                "channel.status",
                json!({"channel": name, "enabled": enabled}),
            )
            .await;
        Ok(json!({"channel": name, "enabled": enabled}))
    }

    pub(crate) async fn channel_config(
        self: &Arc<Self>,
        channel: ChannelKind,
        config: Option<Value>,
    ) -> Result<Value> {
        let name = channel.as_str().to_string();
        let Some(config) = config else {
            return Ok(self
                .profile
                .read()
                .expect("profile lock poisoned")
                .config
                .channels
                .get(&name)
                .cloned()
                .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                .unwrap_or(Value::Null));
        };
        let yaml: serde_yaml::Value = serde_json::from_value(config)?;
        anyhow::ensure!(yaml.is_mapping(), "channel config must be an object");
        self.config_manager
            .update(|profile| {
                anyhow::ensure!(
                    profile.channels.contains_key(&name),
                    "channel is not configured"
                );
                profile.channels.insert(name.clone(), yaml.clone());
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        self.events
            .publish(
                "channel.status",
                json!({"channel": name, "status": "configured"}),
            )
            .await;
        Ok(json!({"channel": name, "updated": true}))
    }

    pub(crate) async fn channel_send_message(
        &self,
        channel: ChannelKind,
        text: String,
    ) -> Result<Value> {
        let target = self.channels().bound_target(channel).await?;
        self.channel_gateway.send_message(channel, &text).await?;
        Ok(json!({"sent": true, "to": target}))
    }

    pub(crate) async fn channel_send_file(
        &self,
        channel: ChannelKind,
        path: PathBuf,
    ) -> Result<Value> {
        let target = self.channels().bound_target(channel).await?;
        self.channel_gateway.send_file(channel, &path).await?;
        Ok(json!({"sent": true, "to": target, "path": path}))
    }

    pub(crate) async fn channel_remove(self: &Arc<Self>, channel: ChannelKind) -> Result<Value> {
        let removed = self.channel_gateway.unbind(channel, self.clone()).await?;
        self.events
            .publish(
                "channel.status",
                json!({"channel": channel.as_str(), "status": "removed"}),
            )
            .await;
        Ok(json!({"removed": removed}))
    }
}
