use std::path::Path;

use serde::{Deserialize, Serialize};

use super::xml_escape;

const CHANNEL_CAPABILITY_DIRECTORY: &str = "runtime/channel-capabilities";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCapabilitySnapshot {
    pub name: String,
    pub content: String,
}

impl ChannelCapabilitySnapshot {
    pub(crate) fn scan(profile_root: &Path) -> Vec<Self> {
        let directory = profile_root.join(CHANNEL_CAPABILITY_DIRECTORY);
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        let mut capabilities = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("md") {
                    return None;
                }
                let name = path.file_stem()?.to_str()?.trim();
                let content = std::fs::read_to_string(&path).ok()?;
                let content = content.trim();
                if name.is_empty() || content.is_empty() {
                    return None;
                }
                Some(Self {
                    name: name.to_string(),
                    content: content.to_string(),
                })
            })
            .collect::<Vec<_>>();
        capabilities.sort_by(|left, right| left.name.cmp(&right.name));
        capabilities
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
