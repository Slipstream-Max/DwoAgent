use anyhow::Result;

use super::{ConfigSnapshot, ConfigUpdateParam, Host};

impl Host {
    pub(crate) fn config_snapshot(&self, session_count: usize) -> ConfigSnapshot {
        let (
            policy,
            default_model,
            default_reasoning,
            models,
            logging,
            external_skills_dirs,
            external_rule_files,
            compaction_trigger_ratio,
            subagent_model,
            subagent_reasoning,
        ) = {
            let profile = self.profile.read().expect("profile lock poisoned");
            (
                profile.config.policy_mode,
                profile.config.model.default.model.clone(),
                profile.config.model.default.reasoning.clone(),
                profile.model_options.clone(),
                profile.config.logging.clone(),
                profile.config.external_skills_dirs.clone(),
                profile.config.external_rule_files.clone(),
                profile.config.model.compaction_trigger_ratio,
                profile
                    .config
                    .model
                    .subagent
                    .as_ref()
                    .map(|subagent| subagent.model.clone()),
                profile
                    .config
                    .model
                    .subagent
                    .as_ref()
                    .and_then(|subagent| subagent.reasoning.clone()),
            )
        };
        ConfigSnapshot {
            policy,
            policy_mode: policy,
            default_model,
            default_reasoning,
            models,
            session_count,
            logging,
            external_skills_dirs,
            external_rule_files,
            compaction_trigger_ratio,
            subagent_model,
            subagent_reasoning,
        }
    }

    pub(crate) async fn update_config(
        self: &std::sync::Arc<Self>,
        params: ConfigUpdateParam,
    ) -> Result<()> {
        self.edit_profile(|profile| {
            if let Some(policy_mode) = params.policy_mode {
                profile.policy_mode = policy_mode;
            }
            if let Some(logging) = params.logging {
                profile.logging = logging;
            }
            if let Some(dirs) = params.external_skills_dirs {
                profile.external_skills_dirs = dirs;
            }
            if let Some(files) = params.external_rule_files {
                profile.external_rule_files = files;
            }
            Ok(())
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn gui_configuration_round_trips_without_exposing_secrets() {
        let root = tempfile::tempdir().unwrap();
        let config = crate::host::tests::write_test_profile(root.path());
        let host = Host::build(&config, dwo_channels::default_factory()).await.unwrap();
        let existing = host
            .create_session(super::super::HostSessionOptions::default())
            .await
            .unwrap();
        host.handle_request(
            "gui-test",
            "config-1",
            "config.update",
            json!({
                "policyMode": "watch",
                "logging": {"level": "debug", "retentionDays": 7},
                "externalSkillsDirs": [],
                "externalRuleFiles": []
            }),
        )
        .await
        .unwrap();
        let snapshot = serde_json::to_value(host.config_snapshot(1)).unwrap();
        assert_eq!(snapshot["policy"], "watch");
        assert_eq!(snapshot["policyMode"], "watch");
        assert_eq!(snapshot["logging"]["retentionDays"], 7);
        assert_eq!(snapshot["externalSkillsDirs"], json!([]));
        assert!(snapshot["compactionTriggerRatio"].is_number());
        assert!(!snapshot.to_string().contains("apiKey"));
        // Changing the default policy must not change an existing session.
        let previous = host
            .handle_request(
                "gui-test",
                "status-1",
                "session.status",
                json!({"session_id": existing}),
            )
            .await
            .unwrap();
        assert_eq!(previous["record"]["info"]["mode"], "confirm");
        let new_id = host
            .create_session(super::super::HostSessionOptions::default())
            .await
            .unwrap();
        let next = host
            .handle_request(
                "gui-test",
                "status-2",
                "session.status",
                json!({"session_id": new_id}),
            )
            .await
            .unwrap();
        assert_eq!(next["record"]["info"]["mode"], "watch");
        host.shutdown().await;
    }
}
