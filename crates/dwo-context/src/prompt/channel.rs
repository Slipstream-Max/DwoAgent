use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use super::xml_escape;

static RUNTIME_CHANNELS: OnceLock<RwLock<BTreeMap<PathBuf, Vec<ChannelCapabilitySnapshot>>>> =
    OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCapabilitySnapshot {
    pub name: String,
    pub content: String,
}

impl ChannelCapabilitySnapshot {
    pub fn set_runtime(profile_root: impl Into<PathBuf>, channels: Vec<Self>) {
        let registry = RUNTIME_CHANNELS.get_or_init(|| RwLock::new(BTreeMap::new()));
        registry
            .write()
            .expect("channel capability registry poisoned")
            .insert(profile_root.into(), channels);
    }

    pub fn runtime(profile_root: &Path) -> Vec<Self> {
        Self::scan(profile_root)
    }

    pub(crate) fn scan(profile_root: &Path) -> Vec<Self> {
        RUNTIME_CHANNELS
            .get()
            .and_then(|registry| registry.read().ok())
            .and_then(|registry| registry.get(profile_root).cloned())
            .unwrap_or_default()
    }

    pub fn render(&self) -> String {
        format!(
            "<channel name=\"{}\" state=\"available\">\n{}\n</channel>",
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
