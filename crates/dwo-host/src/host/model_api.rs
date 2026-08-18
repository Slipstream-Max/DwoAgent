use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use super::Host;

impl Host {
    pub(crate) fn model_list(&self) -> Result<Value> {
        redacted_model_config(
            &self
                .profile
                .read()
                .expect("profile lock poisoned")
                .config
                .model,
        )
    }

    pub(crate) async fn model_set_default(self: &Arc<Self>, name: String) -> Result<Value> {
        self.config_manager
            .update(|profile| {
                profile.model.default_model_name = name.clone();
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({"defaultModelName": name}))
    }

    pub(crate) async fn model_upsert(
        self: &Arc<Self>,
        entry: dwo_agent_service::AgentModelEntry,
    ) -> Result<Value> {
        let name = entry.model_name.clone();
        self.config_manager
            .update(|profile| {
                if let Some(existing) = profile
                    .model
                    .models
                    .iter_mut()
                    .find(|item| item.model_name == name)
                {
                    *existing = entry.clone();
                } else {
                    profile.model.models.push(entry.clone());
                }
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({"model": name, "updated": true}))
    }

    pub(crate) async fn model_remove(self: &Arc<Self>, name: String) -> Result<Value> {
        self.config_manager
            .update(|profile| {
                let previous = profile.model.models.len();
                profile.model.models.retain(|item| item.model_name != name);
                anyhow::ensure!(
                    profile.model.models.len() != previous,
                    "model not found: {name}"
                );
                anyhow::ensure!(
                    profile.model.default_model_name != name,
                    "cannot remove the default model"
                );
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({"model": name, "removed": true}))
    }

    pub(crate) fn provider_list(&self) -> Result<Value> {
        Ok(self
            .model_list()?
            .get("providers")
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new())))
    }

    pub(crate) async fn provider_upsert(
        self: &Arc<Self>,
        name: String,
        provider: dwo_agent_service::AgentProviderConfig,
    ) -> Result<Value> {
        self.config_manager
            .update(|profile| {
                profile
                    .model
                    .providers
                    .insert(name.clone(), provider.clone());
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({"provider": name, "updated": true}))
    }

    pub(crate) async fn provider_remove(self: &Arc<Self>, name: String) -> Result<Value> {
        self.config_manager
            .update(|profile| {
                anyhow::ensure!(
                    profile.model.providers.remove(&name).is_some(),
                    "provider not found: {name}"
                );
                profile.model.models.retain(|item| item.provider != name);
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({"provider": name, "removed": true}))
    }

    pub(crate) fn provider_catalog_list(&self) -> Result<Value> {
        let directory = self.profile_root.join("resource/providers");
        let mut catalog = dwo_agent_service::ModelCatalog::builtin()?;
        catalog.merge_provider_directory(&directory)?;
        super::redacted_provider_catalog(&catalog)
    }

    pub(crate) async fn provider_catalog_upsert(
        self: &Arc<Self>,
        name: String,
        provider: dwo_agent_service::ProviderSpec,
    ) -> Result<Value> {
        super::validate_resource_name(&name)?;
        let builtin = dwo_agent_service::ModelCatalog::builtin()?;
        anyhow::ensure!(
            !builtin.providers.contains_key(&name),
            "built-in provider catalog cannot be replaced: {name}"
        );
        dwo_agent_service::ModelCatalog {
            providers: BTreeMap::from([(name.clone(), provider.clone())]),
        }
        .validate()?;

        let path = self
            .profile_root
            .join("resource/providers")
            .join(format!("{name}.yaml"));
        self.config_manager
            .write_resource(
                &path,
                serde_yaml::to_string(&provider)?.into_bytes(),
                |_| Ok(()),
            )
            .await?;
        self.reload_profile(true).await?;
        Ok(json!({"providerType": name, "updated": true}))
    }

    pub(crate) async fn provider_catalog_remove(self: &Arc<Self>, name: String) -> Result<Value> {
        super::validate_resource_name(&name)?;
        let in_use = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .config
            .model
            .providers
            .values()
            .any(|provider| provider.provider_type == name);
        anyhow::ensure!(!in_use, "provider catalog is in use: {name}");

        let path = self
            .profile_root
            .join("resource/providers")
            .join(format!("{name}.yaml"));
        let removed = self.config_manager.remove_resource_file(&path).await?;
        Ok(json!({"providerType": name, "removed": removed}))
    }
}

fn redacted_model_config(config: &dwo_agent_service::AgentModelConfig) -> Result<Value> {
    super::redacted_model_config(config)
}
