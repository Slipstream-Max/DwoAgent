use anyhow::Result;
use serde_json::{Value, json};

use super::Host;

impl Host {
    pub(crate) fn skill_list(&self) -> Result<Value> {
        let active = self.service.skill_snapshots(&self.profile_root)?;
        let disabled = list_skill_names(&self.profile_root.join("resource/skills.disabled"))?;
        Ok(json!({"skills": active, "disabled": disabled}))
    }

    pub(crate) async fn skill_set_enabled(&self, name: String, enabled: bool) -> Result<Value> {
        super::validate_resource_name(&name)?;
        let root = self.profile_root.join("resource/skills");
        let disabled_root = self.profile_root.join("resource/skills.disabled");
        let (from, to) = if enabled {
            (disabled_root.join(&name), root.join(&name))
        } else {
            (root.join(&name), disabled_root.join(&name))
        };
        anyhow::ensure!(from.is_dir(), "skill not found: {name}");
        self.config_manager
            .move_resource_dir(&from, &to, true)
            .await?;
        self.events
            .publish("skill.changed", json!({"name": name, "enabled": enabled}))
            .await;
        Ok(json!({"name": name, "enabled": enabled}))
    }

    pub(crate) async fn skill_install(&self, name: String, content: String) -> Result<Value> {
        super::validate_resource_name(&name)?;
        let dir = self.profile_root.join("resource/skills").join(&name);
        anyhow::ensure!(!dir.exists(), "skill already exists: {name}");
        let path = dir.join("SKILL.md");
        if let Err(error) = self
            .config_manager
            .write_resource(&path, content.into_bytes(), |_| Ok(()))
            .await
        {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(error);
        }
        if let Err(error) = self.service.skill_snapshots(&self.profile_root) {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(error.into());
        }
        self.events
            .publish("skill.changed", json!({"name": name, "action": "install"}))
            .await;
        Ok(json!({"name": name, "installed": true, "path": path}))
    }

    pub(crate) async fn skill_uninstall(&self, name: String) -> Result<Value> {
        super::validate_resource_name(&name)?;
        let mut removed = false;
        for dir in [
            self.profile_root.join("resource/skills").join(&name),
            self.profile_root
                .join("resource/skills.disabled")
                .join(&name),
        ] {
            removed |= self.config_manager.remove_resource_dir(&dir).await?;
        }
        if removed {
            self.events
                .publish(
                    "skill.changed",
                    json!({"name": name, "action": "uninstall"}),
                )
                .await;
        }
        Ok(json!({"name": name, "removed": removed}))
    }
}

fn list_skill_names(root: &std::path::Path) -> Result<Vec<String>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}
