use anyhow::Result;

use super::{ConfigSnapshot, ConfigUpdateParam, Host};

impl Host {
    pub(crate) fn config_snapshot(&self, session_count: usize) -> ConfigSnapshot {
        let (policy, default_model, default_reasoning, models, max_model_steps) = {
            let profile = self.profile.read().expect("profile lock poisoned");
            (
                profile.config.policy_mode,
                profile.config.model.default.model.clone(),
                profile.config.model.default.reasoning.clone(),
                profile.model_options.clone(),
                profile.config.max_model_steps,
            )
        };
        ConfigSnapshot {
            policy,
            default_model,
            default_reasoning,
            models,
            max_model_steps,
            session_count,
        }
    }

    pub(crate) async fn update_config(
        self: &std::sync::Arc<Self>,
        params: ConfigUpdateParam,
    ) -> Result<()> {
        self.config_manager
            .update(|profile| {
                if let Some(max_model_steps) = params.max_model_steps {
                    profile.max_model_steps = max_model_steps;
                }
                if let Some(logging) = params.logging {
                    profile.logging = logging;
                }
                if let Some(dirs) = params.external_skills_dirs {
                    profile.external_skills_dirs = dirs;
                }
                Ok(())
            })
            .await?;
        self.reload_profile_if_changed().await?;
        Ok(())
    }
}
