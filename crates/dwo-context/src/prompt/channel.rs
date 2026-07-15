use std::path::Path;

use serde::{Deserialize, Serialize};

use super::xml_escape;

const WEIXIN_PROMPT: &str = r#"A Weixin channel is bound. Your normal reasoning and responses are already streamed to the user through Weixin.

Do not use the proactive messaging commands for normal replies.
Only use `dwo channel weixin send-message <message>` when the user explicitly asks you to proactively send a specific message.
Only use `dwo channel weixin send-file <path>` when the user explicitly asks you to send a file.

Use `dwo channel weixin --help` to inspect the available commands."#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCapabilitySnapshot {
    pub name: String,
    pub content: String,
}

impl ChannelCapabilitySnapshot {
    pub(crate) fn scan(profile_root: &Path) -> Vec<Self> {
        if weixin_is_available(profile_root) {
            vec![Self {
                name: "weixin".to_string(),
                content: WEIXIN_PROMPT.to_string(),
            }]
        } else {
            Vec::new()
        }
    }

    pub fn render(&self) -> String {
        format!(
            "<channel name=\"{}\">\n{}\n</channel>",
            xml_escape(&self.name),
            xml_escape(&self.content)
        )
    }

    pub fn render_removed(name: &str) -> String {
        format!(
            "<channel name=\"{}\" state=\"removed\">\nThe channel is no longer available.\n</channel>",
            xml_escape(name)
        )
    }
}

fn weixin_is_available(profile_root: &Path) -> bool {
    let profile = std::fs::read_to_string(profile_root.join("profile.yaml"))
        .ok()
        .and_then(|source| serde_yaml::from_str::<serde_yaml::Value>(&source).ok());
    let enabled = profile
        .as_ref()
        .and_then(|profile| profile.get("channels"))
        .and_then(|channels| channels.get("weixin"))
        .and_then(|weixin| weixin.get("enabled"))
        .and_then(serde_yaml::Value::as_bool)
        == Some(true);
    enabled && valid_weixin_secret(&profile_root.join("channels/weixin/secret.yaml"))
}

fn valid_weixin_secret(path: &Path) -> bool {
    let secret = std::fs::read_to_string(path)
        .ok()
        .and_then(|source| serde_yaml::from_str::<serde_yaml::Value>(&source).ok());
    ["botToken", "baseUrl", "ilinkBotId", "boundUserId"]
        .into_iter()
        .all(|field| {
            secret
                .as_ref()
                .and_then(|secret| secret.get(field))
                .and_then(serde_yaml::Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
        })
}
