use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use super::Host;

impl Host {
    pub(crate) fn model_list(&self) -> Result<Value> {
        super::redacted_model_config(
            &self
                .profile
                .read()
                .expect("profile lock poisoned")
                .config
                .model,
        )
    }

    pub(crate) async fn model_set_default(
        self: &Arc<Self>,
        default: dwo_agent_service::DefaultModelConfig,
    ) -> Result<Value> {
        self.config_manager
            .update(|profile| {
                profile.model.default = default.clone();
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({"default": default}))
    }

    pub(crate) async fn model_upsert(
        self: &Arc<Self>,
        provider_name: String,
        model_name: String,
        entry: dwo_agent_service::AgentModelEntry,
    ) -> Result<Value> {
        let mut catalog = dwo_agent_service::ModelCatalog::builtin()?;
        catalog.merge_model_directory(self.profile_root.join("resource/models"))?;
        self.config_manager
            .update(|profile| {
                let provider = profile
                    .model
                    .providers
                    .get_mut(&provider_name)
                    .ok_or_else(|| anyhow::anyhow!("provider not found: {provider_name}"))?;
                if provider.models.is_none() {
                    let family = catalog.families.get(&provider_name).ok_or_else(|| {
                        anyhow::anyhow!("provider {provider_name} has no implicit model family")
                    })?;
                    provider.models = Some(
                        family
                            .models
                            .keys()
                            .map(|id| {
                                (
                                    id.clone(),
                                    dwo_agent_service::AgentModelEntry {
                                        model_id: None,
                                        profile: None,
                                        context_window_tokens: None,
                                        max_output_tokens: None,
                                        default_reasoning_mode: None,
                                        capabilities: None,
                                        reasoning: None,
                                        hosted_tools: None,
                                        temperature: None,
                                        top_p: None,
                                        extra_body: serde_json::Map::new(),
                                    },
                                )
                            })
                            .collect(),
                    );
                }
                let models = provider.models.as_mut().expect("models materialized");
                let model_id = entry.effective_model_id(&model_name).to_string();
                models.retain(|name, model| {
                    name == &model_name || model.effective_model_id(name) != model_id
                });
                models.insert(model_name.clone(), entry.clone());
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({
            "provider": provider_name,
            "modelName": model_name,
            "updated": true
        }))
    }

    pub(crate) async fn model_remove(
        self: &Arc<Self>,
        provider_name: String,
        model_id: String,
    ) -> Result<Value> {
        let stable_id = format!("{provider_name}/{model_id}");
        self.config_manager
            .update(|profile| {
                anyhow::ensure!(
                    profile.model.default.model != stable_id,
                    "cannot remove the default model"
                );
                let provider = profile
                    .model
                    .providers
                    .get_mut(&provider_name)
                    .ok_or_else(|| anyhow::anyhow!("provider not found: {provider_name}"))?;
                let models = provider.models.as_mut().ok_or_else(|| {
                    anyhow::anyhow!(
                        "provider {provider_name} uses its implicit model list; configure models before removing one"
                    )
                })?;
                let previous = models.len();
                models.retain(|name, model| model.effective_model_id(name) != model_id);
                anyhow::ensure!(models.len() != previous, "model not found: {stable_id}");
                anyhow::ensure!(!models.is_empty(), "provider models must not be empty");
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({"model": stable_id, "removed": true}))
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
                    !profile.model.default.model.starts_with(&format!("{name}/")),
                    "cannot remove the default model provider"
                );
                anyhow::ensure!(
                    profile.model.providers.shift_remove(&name).is_some(),
                    "provider not found: {name}"
                );
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(json!({"provider": name, "removed": true}))
    }

    pub(crate) fn model_catalog_list(&self) -> Result<Value> {
        let mut catalog = dwo_agent_service::ModelCatalog::builtin()?;
        catalog.merge_model_directory(self.profile_root.join("resource/models"))?;
        Ok(serde_json::to_value(catalog)?)
    }

    pub(crate) async fn model_catalog_upsert(
        self: &Arc<Self>,
        family: String,
        spec: dwo_agent_service::ModelFamilySpec,
    ) -> Result<Value> {
        super::validate_resource_name(&family)?;
        let mut catalog = dwo_agent_service::ModelCatalog::builtin()?;
        catalog.merge_model_directory(self.profile_root.join("resource/models"))?;
        catalog.merge_family(family.clone(), spec.clone())?;
        self.profile
            .read()
            .expect("profile lock poisoned")
            .config
            .resolve_models(&catalog)?;

        let path = self
            .profile_root
            .join("resource/models")
            .join(format!("{family}.yaml"));
        self.config_manager
            .write_resource(
                &path,
                serde_yaml::to_string(&spec)?.into_bytes(),
                |_| Ok(()),
            )
            .await?;
        self.reload_profile(true).await?;
        Ok(json!({"family": family, "updated": true}))
    }

    pub(crate) async fn model_catalog_remove(self: &Arc<Self>, family: String) -> Result<Value> {
        super::validate_resource_name(&family)?;
        let builtin = dwo_agent_service::ModelCatalog::builtin()?;
        if !builtin.families.contains_key(&family) {
            let in_use = self
                .profile
                .read()
                .expect("profile lock poisoned")
                .config
                .model
                .providers
                .iter()
                .any(|(provider_name, provider)| {
                    (provider_name == &family && provider.models.is_none())
                        || provider.models.as_ref().is_some_and(|models| {
                            models.values().any(|model| {
                                model.profile.as_deref().is_some_and(|profile| {
                                    profile.starts_with(&format!("{family}/"))
                                })
                            })
                        })
                });
            anyhow::ensure!(!in_use, "model family is in use: {family}");
        }

        let path = self
            .profile_root
            .join("resource/models")
            .join(format!("{family}.yaml"));
        let removed = self.config_manager.remove_resource_file(&path).await?;
        if removed {
            self.reload_profile(true).await?;
        }
        Ok(json!({"family": family, "removed": removed}))
    }
}
