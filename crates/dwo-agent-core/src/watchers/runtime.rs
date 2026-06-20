//! Watcher runtime and pending watcher-content queue.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::env_block::EnvBlockWatcher;

const ENV_BLOCK_KEY: &str = "env_block";
const WATCHER_CONTENT_OPEN: &str = "<watcher_content>";
const WATCHER_CONTENT_CLOSE: &str = "</watcher_content>";

pub struct WatcherRuntime {
    pending: Arc<Mutex<BTreeMap<String, String>>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl WatcherRuntime {
    pub fn start_env_block(watcher: EnvBlockWatcher, interval: Duration) -> Arc<Self> {
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let cancel = CancellationToken::new();
        let task = spawn_env_block_task(watcher, interval, pending.clone(), cancel.clone());
        Arc::new(Self {
            pending,
            cancel,
            task,
        })
    }

    pub async fn drain_pending_messages(&self) -> Vec<Value> {
        let drained = {
            let mut pending = self.pending.lock().await;
            std::mem::take(&mut *pending)
        };

        drained
            .into_values()
            .map(|content| {
                json!({
                    "role": "system",
                    "content": wrap_watcher_content(&content),
                })
            })
            .collect()
    }
}

impl Drop for WatcherRuntime {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

fn spawn_env_block_task(
    watcher: EnvBlockWatcher,
    interval: Duration,
    pending: Arc<Mutex<BTreeMap<String, String>>>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_content = match watcher.build_content() {
            Ok(content) => Some(content),
            Err(err) => {
                warn!("initial env block watcher build failed: {err:#}");
                None
            }
        };

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => {}
            }

            let content = match watcher.build_content() {
                Ok(content) => content,
                Err(err) => {
                    warn!("env block watcher build failed: {err:#}");
                    continue;
                }
            };

            if last_content.as_deref() == Some(content.as_str()) {
                continue;
            }

            last_content = Some(content.clone());
            pending
                .lock()
                .await
                .insert(ENV_BLOCK_KEY.to_string(), content);
        }
    })
}

fn wrap_watcher_content(content: &str) -> String {
    format!(
        "{WATCHER_CONTENT_OPEN}\n{}\n{WATCHER_CONTENT_CLOSE}",
        content.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watchers::env_block::EnvBlockWatcher;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn env_block_watcher_queues_changed_snapshot() {
        let temp = tempdir().unwrap();
        let agent_dir = temp.path().join("agent");
        let skills_dir = agent_dir.join("resources").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let prompt_dir = agent_dir.join("resources").join("prompt");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        std::fs::write(prompt_dir.join("system.md"), "test").unwrap();

        let watcher = EnvBlockWatcher::new(
            agent_dir.clone(),
            "test-agent",
            temp.path().to_string_lossy(),
            Vec::new(),
            Vec::new(),
        );
        let runtime = WatcherRuntime::start_env_block(watcher, Duration::from_millis(50));
        tokio::time::sleep(Duration::from_millis(75)).await;

        let new_skill = skills_dir.join("new-skill");
        std::fs::create_dir_all(&new_skill).unwrap();
        std::fs::write(
            new_skill.join("SKILL.md"),
            "---\nname: new-skill\ndescription: Newly added skill\n---\n",
        )
        .unwrap();
        std::fs::write(agent_dir.join("resources").join("mcp.json"), "{}").unwrap();

        tokio::time::sleep(Duration::from_millis(125)).await;
        let messages = runtime.drain_pending_messages().await;
        let content = messages
            .first()
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("");

        assert_eq!(messages.len(), 1);
        assert!(content.contains("<watcher_content>"));
        assert!(content.contains("<env_block>"));
        assert!(content.contains("<name>\nnew-skill\n</name>"));
        assert!(content.contains("<mcp>"));
        assert!(content.contains("mcporter --version"));
        assert!(content.contains("mcporter --config"));
    }
}
