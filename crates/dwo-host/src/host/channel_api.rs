use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::Host;
use dwo_channel_api::{ChannelControl, ChannelGatewayApi, ChannelKind, ChannelPollParams};

#[derive(serde::Deserialize)]
struct SendMessageParam {
    text: String,
}

#[derive(serde::Deserialize)]
struct SendFileParam {
    path: PathBuf,
}

#[derive(serde::Deserialize)]
struct ChannelTokenParam {
    token: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelCredentialsParam {
    app_id: String,
    app_secret: String,
}

#[derive(Clone, Copy)]
pub(crate) enum ManagedChannelAction {
    Status,
    Enable,
    Disable,
    Config,
    Token,
    Credentials,
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
        "token" => ManagedChannelAction::Token,
        "credentials" => ManagedChannelAction::Credentials,
        "send_message" => ManagedChannelAction::SendMessage,
        "send_file" => ManagedChannelAction::SendFile,
        "remove" => ManagedChannelAction::Remove,
        _ => return None,
    };
    Some((channel, action))
}

impl Host {
    pub(crate) fn channels(&self) -> Arc<dyn ChannelControl> {
        self.channels
            .read()
            .expect("channel manager lock poisoned")
            .clone()
    }

    pub(crate) fn gateway(&self) -> Arc<dyn ChannelGatewayApi> {
        self.channel_gateway
            .read()
            .expect("channel gateway lock poisoned")
            .clone()
    }

    pub(crate) async fn dispatch_channel(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if let Some((channel, action)) = managed_channel_action(method) {
            return match action {
                ManagedChannelAction::Status => self.channel_status(channel).await,
                ManagedChannelAction::Enable | ManagedChannelAction::Disable => {
                    self.channel_set_enabled(
                        channel,
                        matches!(action, ManagedChannelAction::Enable),
                    )
                    .await
                }
                ManagedChannelAction::Config => {
                    self.channel_config(channel, params.get("config").cloned())
                        .await
                }
                ManagedChannelAction::Token => {
                    let params: ChannelTokenParam = serde_json::from_value(params)?;
                    self.channel_save_token(channel, &params.token).await
                }
                ManagedChannelAction::Credentials => {
                    let params: ChannelCredentialsParam = serde_json::from_value(params)?;
                    self.channel_save_credentials(channel, &params.app_id, &params.app_secret)
                        .await
                }
                ManagedChannelAction::SendMessage => {
                    let params: SendMessageParam = serde_json::from_value(params)?;
                    self.channel_send_message(channel, params.text).await
                }
                ManagedChannelAction::SendFile => {
                    let params: SendFileParam = serde_json::from_value(params)?;
                    self.channel_send_file(channel, params.path).await
                }
                ManagedChannelAction::Remove => self.channel_remove(channel).await,
            };
        }
        if method == "channel.list" {
            return self.channel_list().await;
        }
        let mut parts = method.split('.');
        anyhow::ensure!(
            parts.next() == Some("channel"),
            "invalid channel method: {method}"
        );
        let channel_name = parts
            .next()
            .with_context(|| format!("missing channel name in method: {method}"))?;
        let action = parts
            .next()
            .with_context(|| format!("missing channel action in method: {method}"))?;
        anyhow::ensure!(parts.next().is_none(), "invalid channel method: {method}");
        let channel = ChannelKind::parse(channel_name)
            .with_context(|| format!("unknown channel in method: {method}"))?;
        match action {
            "begin" | "bind" => self.channel_begin_bind(channel).await,
            "poll" => {
                let params: ChannelPollParams = serde_json::from_value(params)?;
                self.channel_poll_bind(channel, params).await
            }
            "unbind" => self.channel_unbind(channel).await,
            other => anyhow::bail!("unknown channel action: {other}"),
        }
    }

    pub(crate) async fn channel_list(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.channels().list().await?)?)
    }

    pub(crate) async fn channel_begin_bind(
        self: &Arc<Self>,
        channel: ChannelKind,
    ) -> Result<Value> {
        let result = self
            .gateway()
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
            self.gateway()
                .poll_bind(channel, self.clone(), params)
                .await?,
        )?)
    }

    pub(crate) async fn channel_unbind(self: &Arc<Self>, channel: ChannelKind) -> Result<Value> {
        let unbound = self.gateway().unbind(channel, self.clone()).await?;
        // 解绑即关：不留 `enabled=true` 的孤儿位（profile 不变量）。
        self.disable_after_unbind(channel).await?;
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

    /// 解绑 / 移除之后把 `enabled` 落回 false；渠道根本没配置就啥也不用做。
    async fn disable_after_unbind(self: &Arc<Self>, channel: ChannelKind) -> Result<()> {
        let configured = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .config
            .channels
            .contains_key(channel.as_str());
        if configured {
            self.channel_set_enabled(channel, false).await?;
        }
        Ok(())
    }

    pub(crate) async fn channel_set_enabled(
        self: &Arc<Self>,
        channel: ChannelKind,
        enabled: bool,
    ) -> Result<Value> {
        let name = channel.as_str().to_string();
        // 不变量：enable 只允许用在**绑定过**的渠道上（否则就是那个
        // 「开着但啥也干不了」的骗人位）——和 profile 加载时的校验一个口径。
        if enabled {
            let summary = self.channels().summary(channel).await?;
            anyhow::ensure!(
                summary.bound_user_id.is_some(),
                "channel is not bound: bind it before enabling"
            );
        }
        self.edit_profile(|profile| {
            // 管理面不要求先手写 profile 段：没配置就从默认配置开始。
            let entry = profile
                .channels
                .entry(name.clone())
                .or_insert_with(|| dwo_channel_api::default_channel_config(&name));
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
            // 未配置的渠道按默认配置回：管理面直接打开设置页就能继续配置。
            let value = self
                .profile
                .read()
                .expect("profile lock poisoned")
                .config
                .channels
                .get(&name)
                .cloned()
                .unwrap_or_else(|| dwo_channel_api::default_channel_config(&name));
            return Ok(serde_json::to_value(value).unwrap_or(Value::Null));
        };
        let yaml: serde_yaml::Value = serde_json::from_value(config)?;
        anyhow::ensure!(yaml.is_mapping(), "channel config must be an object");
        // 写回整条配置也不行绕过「enable 要绑定」那道门。
        if yaml.get("enabled").and_then(|value| value.as_bool()) == Some(true) {
            let summary = self.channels().summary(channel).await?;
            anyhow::ensure!(
                summary.bound_user_id.is_some(),
                "channel is not bound: cannot write enabled=true"
            );
        }
        self.edit_profile(|profile| {
            profile.channels.insert(name.clone(), yaml.clone());
            Ok(())
        })
        .await?;
        self.events
            .publish(
                "channel.status",
                json!({"channel": name, "status": "configured"}),
            )
            .await;
        Ok(json!({"channel": name, "updated": true}))
    }

    pub(crate) async fn channel_save_token(
        &self,
        channel: ChannelKind,
        token: &str,
    ) -> Result<Value> {
        anyhow::ensure!(!token.trim().is_empty(), "token must not be empty");
        self.gateway().save_token(channel, token).await?;
        Ok(json!({"saved": true}))
    }

    pub(crate) async fn channel_save_credentials(
        &self,
        channel: ChannelKind,
        app_id: &str,
        app_secret: &str,
    ) -> Result<Value> {
        anyhow::ensure!(
            channel == ChannelKind::Feishu,
            "channel {} does not support credential storage",
            channel.as_str()
        );
        anyhow::ensure!(
            !app_id.trim().is_empty() && !app_secret.trim().is_empty(),
            "app id / secret must not be empty"
        );
        self.gateway()
            .save_feishu_credentials(app_id, app_secret)
            .await?;
        Ok(json!({"saved": true}))
    }

    pub(crate) async fn channel_send_message(
        &self,
        channel: ChannelKind,
        text: String,
    ) -> Result<Value> {
        let target = self.channels().bound_target(channel).await?;
        self.gateway().send_message(channel, &text).await?;
        Ok(json!({"sent": true, "to": target}))
    }

    pub(crate) async fn channel_send_file(
        &self,
        channel: ChannelKind,
        path: PathBuf,
    ) -> Result<Value> {
        let target = self.channels().bound_target(channel).await?;
        self.gateway().send_file(channel, &path).await?;
        Ok(json!({"sent": true, "to": target, "path": path}))
    }

    pub(crate) async fn channel_remove(self: &Arc<Self>, channel: ChannelKind) -> Result<Value> {
        let removed = self.gateway().unbind(channel, self.clone()).await?;
        // 同上：没有绑定就不许留着 `enabled=true`。
        self.disable_after_unbind(channel).await?;
        self.events
            .publish(
                "channel.status",
                json!({"channel": channel.as_str(), "status": "removed"}),
            )
            .await;
        Ok(json!({"removed": removed}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_channel_actions_share_one_rpc_route() {
        for channel in ChannelKind::ALL {
            let method = |action| format!("channel.{}.{action}", channel.as_str());
            assert!(matches!(
                managed_channel_action(&method("status")),
                Some((found, ManagedChannelAction::Status)) if found == channel
            ));
            assert!(matches!(
                managed_channel_action(&method("send_message")),
                Some((found, ManagedChannelAction::SendMessage)) if found == channel
            ));
            assert!(matches!(
                managed_channel_action(&method("send_file")),
                Some((found, ManagedChannelAction::SendFile)) if found == channel
            ));
            assert!(matches!(
                managed_channel_action(&method("remove")),
                Some((found, ManagedChannelAction::Remove)) if found == channel
            ));
            assert!(matches!(
                managed_channel_action(&method("token")),
                Some((found, ManagedChannelAction::Token)) if found == channel
            ));
            assert!(managed_channel_action(&method("begin")).is_none());
        }
    }
}
