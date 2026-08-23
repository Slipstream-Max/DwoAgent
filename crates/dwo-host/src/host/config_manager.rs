use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use dwo_agent_service::{AgentProfileConfig, LoadedAgentProfile};
use tokio::sync::Mutex;

/// The single write boundary for profile-owned configuration.
///
/// Host watchers and management RPCs both go through this type so a partially-written
/// `profile.yaml` can never become the active configuration.
pub(crate) struct ConfigManager {
    root: PathBuf,
    path: PathBuf,
    lock: Mutex<()>,
}

impl ConfigManager {
    pub(crate) fn new(root: PathBuf) -> Self {
        let path = root.join("profile.yaml");
        Self {
            root,
            path,
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn fingerprint(&self) -> Result<String> {
        let mut files = vec![self.path.clone()];
        collect_files(&self.root.join("resource"), &mut files)?;
        files.sort();

        let mut hasher = DefaultHasher::new();
        for path in files {
            path.strip_prefix(&self.root)
                .unwrap_or(&path)
                .hash(&mut hasher);
            std::fs::read(&path)
                .with_context(|| format!("read Host configuration resource {}", path.display()))?
                .hash(&mut hasher);
        }
        Ok(format!("{:016x}", hasher.finish()))
    }

    pub(crate) fn load(&self) -> Result<LoadedAgentProfile> {
        Ok(LoadedAgentProfile::load(&self.root)?)
    }

    pub(crate) async fn update<F>(&self, update: F) -> Result<AgentProfileConfig>
    where
        F: FnOnce(&mut AgentProfileConfig) -> Result<()>,
    {
        let _guard = self.lock.lock().await;
        let mut config = AgentProfileConfig::load(&self.root)?;
        update(&mut config)?;
        config.validate()?;
        let mut catalog = dwo_agent_service::ModelCatalog::builtin()?;
        catalog.merge_model_directory(self.root.join("resource/models"))?;
        config.resolve_models(&catalog)?;
        let source = serde_yaml::to_string(&config)?;
        dwo_agent_service::atomic_file::write(&self.path, source.into_bytes()).await?;
        Ok(config)
    }

    pub(crate) async fn write_resource<F>(
        &self,
        path: &Path,
        bytes: Vec<u8>,
        validate: F,
    ) -> Result<()>
    where
        F: FnOnce(&[u8]) -> Result<()>,
    {
        self.ensure_managed_path(path)?;
        let _guard = self.lock.lock().await;
        validate(&bytes)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        dwo_agent_service::atomic_file::write(path, bytes).await?;
        Ok(())
    }

    pub(crate) async fn move_resource_dir(
        &self,
        from: &Path,
        to: &Path,
        replace: bool,
    ) -> Result<()> {
        self.ensure_managed_path(from)?;
        self.ensure_managed_path(to)?;
        let _guard = self.lock.lock().await;
        anyhow::ensure!(
            from.is_dir(),
            "resource directory not found: {}",
            from.display()
        );
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if to.exists() {
            anyhow::ensure!(replace, "resource already exists: {}", to.display());
            tokio::fs::remove_dir_all(to).await?;
        }
        tokio::fs::rename(from, to).await?;
        Ok(())
    }

    pub(crate) async fn remove_resource_file(&self, path: &Path) -> Result<bool> {
        self.ensure_managed_path(path)?;
        let _guard = self.lock.lock().await;
        if !path.is_file() {
            return Ok(false);
        }
        tokio::fs::remove_file(path).await?;
        Ok(true)
    }

    pub(crate) async fn remove_resource_dir(&self, path: &Path) -> Result<bool> {
        self.ensure_managed_path(path)?;
        let _guard = self.lock.lock().await;
        if !path.is_dir() {
            return Ok(false);
        }
        tokio::fs::remove_dir_all(path).await?;
        Ok(true)
    }

    fn ensure_managed_path(&self, path: &Path) -> Result<()> {
        let relative = path.strip_prefix(&self.root).with_context(|| {
            format!("resource path escapes Host config root: {}", path.display())
        })?;
        anyhow::ensure!(
            !relative.as_os_str().is_empty(),
            "Host config root is not a resource"
        );
        anyhow::ensure!(
            relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
            "resource path contains an invalid component: {}",
            path.display()
        );
        Ok(())
    }
}

fn collect_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resource_writes_stay_inside_host_config_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let manager = ConfigManager::new(root.path().to_path_buf());
        let managed = root.path().join("resource/prompts/System.md");

        manager
            .write_resource(&managed, b"prompt".to_vec(), |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(managed).unwrap(), "prompt");

        let escaped = outside.path().join("System.md");
        assert!(
            manager
                .write_resource(&escaped, b"bad".to_vec(), |_| Ok(()))
                .await
                .is_err()
        );
        assert!(!escaped.exists());
    }

    #[test]
    fn fingerprint_tracks_profile_and_resource_changes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("profile.yaml"), "policyMode: confirm\n").unwrap();
        let manager = ConfigManager::new(root.path().to_path_buf());
        let initial = manager.fingerprint().unwrap();

        let prompt = root.path().join("resource/prompts/System.md");
        std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
        std::fs::write(&prompt, "first").unwrap();
        let with_prompt = manager.fingerprint().unwrap();
        assert_ne!(initial, with_prompt);

        std::fs::write(prompt, "second").unwrap();
        assert_ne!(with_prompt, manager.fingerprint().unwrap());
    }
}
