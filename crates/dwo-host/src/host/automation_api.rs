use anyhow::{Context, Result};
use dwo_tools::SessionMode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use super::Host;
use crate::automation::{AutomationConfig, AutomationJob};

#[derive(Deserialize)]
pub(crate) struct ProjectParam {
    pub(crate) project_id: String,
}

#[derive(Deserialize)]
pub(crate) struct JobParam {
    pub(crate) project_id: String,
    pub(crate) job: String,
}

#[derive(Deserialize)]
pub(crate) struct AddParam {
    pub(crate) project_id: String,
    pub(crate) job: AutomationJob,
}

#[derive(Deserialize)]
pub(crate) struct ToggleParam {
    pub(crate) project_id: String,
    pub(crate) job: Option<String>,
    #[serde(default)]
    pub(crate) all: bool,
}

#[derive(Deserialize)]
pub(crate) struct UpdateParam {
    pub(crate) project_id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) job: Option<AutomationJob>,
    #[serde(default)]
    pub(crate) prompt: Option<String>,
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
    #[serde(default)]
    pub(crate) model: Option<Option<String>>,
    #[serde(default)]
    pub(crate) reasoning: Option<Option<String>>,
    #[serde(default)]
    pub(crate) policy: Option<SessionMode>,
}

#[derive(Deserialize)]
pub(crate) struct HistoryParam {
    pub(crate) project_id: String,
    #[serde(default)]
    pub(crate) job: Option<String>,
    #[serde(default = "default_history_limit")]
    pub(crate) limit: usize,
}

#[derive(Deserialize)]
pub(crate) struct RunParam {
    pub(crate) project_id: String,
    pub(crate) job: String,
    pub(crate) caller_session_id: Option<String>,
}

fn default_history_limit() -> usize {
    50
}

impl Host {
    pub(crate) async fn dispatch_automation(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        match method {
            "automation.list" => {
                let params: ProjectParam = serde_json::from_value(params)?;
                self.automation_list(params.project_id).await
            }
            "automation.status" => {
                let params: JobParam = serde_json::from_value(params)?;
                self.automation_status(params.project_id, params.job).await
            }
            "automation.update" => {
                let params: UpdateParam = serde_json::from_value(params)?;
                self.automation_update(params).await
            }
            "automation.history" => {
                let params: HistoryParam = serde_json::from_value(params)?;
                self.automation_history(params.project_id, params.job, params.limit)
                    .await
            }
            "automation.add" => {
                let params: AddParam = serde_json::from_value(params)?;
                self.automation_add(params.project_id, params.job).await
            }
            "automation.enable" | "automation.disable" => {
                let params: ToggleParam = serde_json::from_value(params)?;
                self.automation_set_enabled(
                    params.project_id,
                    params.job,
                    params.all,
                    method == "automation.enable",
                )
                .await
            }
            "automation.delete" => {
                let params: ToggleParam = serde_json::from_value(params)?;
                self.automation_delete(params.project_id, params.job, params.all)
                    .await
            }
            "automation.run" => {
                let params: RunParam = serde_json::from_value(params)?;
                self.automation_run(params.project_id, params.job, params.caller_session_id)
                    .await
            }
            other => anyhow::bail!("unknown automation method: {other}"),
        }
    }

    pub(crate) async fn automation_list(&self, project_id: String) -> Result<Value> {
        self.projects.get(&project_id)?;
        Ok(serde_json::to_value(
            self.automation.list(Some(&project_id)).await,
        )?)
    }

    pub(crate) async fn automation_status(&self, project_id: String, job: String) -> Result<Value> {
        Ok(serde_json::to_value(
            self.automation.status(&project_id, &job).await?,
        )?)
    }

    pub(crate) async fn automation_update(self: &Arc<Self>, params: UpdateParam) -> Result<Value> {
        let project_id = params.project_id.clone();
        let name = params.name.clone();
        self.mutate_automation_config(&project_id, |config| {
            let job = config
                .jobs
                .iter_mut()
                .find(|job| job.name == name)
                .with_context(|| format!("automation job not found: {name}"))?;
            if let Some(replacement) = params.job {
                anyhow::ensure!(replacement.name == name, "automation name cannot change");
                *job = replacement;
            }
            if let Some(prompt) = params.prompt {
                job.prompt = prompt;
            }
            if let Some(enabled) = params.enabled {
                job.enabled = enabled;
            }
            if let Some(model) = params.model {
                job.model = model;
            }
            if let Some(reasoning) = params.reasoning {
                job.reasoning = reasoning;
            }
            if let Some(policy) = params.policy {
                job.policy = Some(policy);
            }
            Ok(())
        })
        .await?;
        self.events
            .publish(
                "automation.changed",
                json!({"projectId": project_id, "job": name, "action": "update"}),
            )
            .await;
        Ok(serde_json::to_value(
            self.automation.status(&project_id, &name).await?,
        )?)
    }

    pub(crate) async fn automation_history(
        &self,
        project_id: String,
        job: Option<String>,
        limit: usize,
    ) -> Result<Value> {
        self.projects.get(&project_id)?;
        Ok(json!({"runs": self.automation.history(&project_id, job.as_deref(), limit).await}))
    }

    pub(crate) async fn automation_add(
        self: &Arc<Self>,
        project_id: String,
        job: AutomationJob,
    ) -> Result<Value> {
        let name = job.name.clone();
        self.mutate_automation_config(&project_id, |config| {
            anyhow::ensure!(
                !config.jobs.iter().any(|existing| existing.name == name),
                "automation job already exists: {name}"
            );
            config.enabled = true;
            config.jobs.push(job);
            Ok(())
        })
        .await?;
        self.events
            .publish(
                "automation.changed",
                json!({"projectId": project_id, "job": name, "action": "add"}),
            )
            .await;
        Ok(serde_json::to_value(
            self.automation.status(&project_id, &name).await?,
        )?)
    }

    pub(crate) async fn automation_set_enabled(
        self: &Arc<Self>,
        project_id: String,
        job: Option<String>,
        all: bool,
        enabled: bool,
    ) -> Result<Value> {
        anyhow::ensure!(all ^ job.is_some(), "specify a job or --all");
        let event_job = job.clone();
        self.mutate_automation_config(&project_id, |config| {
            if enabled {
                config.enabled = true;
            }
            if all {
                for job in &mut config.jobs {
                    job.enabled = enabled;
                }
            } else if let Some(name) = &job {
                let job = config
                    .jobs
                    .iter_mut()
                    .find(|job| &job.name == name)
                    .with_context(|| format!("automation job not found: {name}"))?;
                job.enabled = enabled;
            }
            Ok(())
        })
        .await?;
        self.events
            .publish(
                "automation.changed",
                json!({"projectId": project_id, "job": event_job, "all": all, "action": if enabled { "enable" } else { "disable" }}),
            )
            .await;
        Ok(json!({
            "updated": if all { "all" } else { job.as_deref().unwrap_or_default() },
            "enabled": enabled
        }))
    }

    pub(crate) async fn automation_delete(
        self: &Arc<Self>,
        project_id: String,
        job: Option<String>,
        all: bool,
    ) -> Result<Value> {
        anyhow::ensure!(all ^ job.is_some(), "specify a job or --all");
        let event_job = job.clone();
        self.mutate_automation_config(&project_id, |config| {
            if all {
                config.jobs.clear();
            } else if let Some(name) = &job {
                let previous = config.jobs.len();
                config.jobs.retain(|job| &job.name != name);
                anyhow::ensure!(
                    config.jobs.len() != previous,
                    "automation job not found: {name}"
                );
            }
            Ok(())
        })
        .await?;
        self.automation
            .remove_job_state(&project_id, job.as_deref(), all)
            .await?;
        self.events
            .publish(
                "automation.changed",
                json!({"projectId": project_id, "job": event_job, "all": all, "action": "delete"}),
            )
            .await;
        Ok(json!({"deleted": if all { "all" } else { job.as_deref().unwrap_or_default() }}))
    }

    pub(crate) async fn automation_run(
        self: &Arc<Self>,
        project_id: String,
        job: String,
        caller_session_id: Option<String>,
    ) -> Result<Value> {
        let caller = super::session_api::parse_optional_session(caller_session_id)?;
        if let Some(caller) = &caller {
            self.service.load(caller).await?;
        }
        let run = self.automation.run_now(&project_id, &job, caller).await?;
        self.events
            .publish(
                "automation.run",
                json!({"projectId": project_id, "job": job, "runId": run.run_id}),
            )
            .await;
        Ok(serde_json::to_value(run)?)
    }

    async fn mutate_automation_config<F>(&self, project_id: &str, update: F) -> Result<()>
    where
        F: FnOnce(&mut AutomationConfig) -> Result<()>,
    {
        self.projects.get(project_id)?;
        self.automation
            .update_project_config(project_id, update)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::tests::write_test_profile;

    #[tokio::test]
    async fn automation_crud_updates_project_and_runtime_together() {
        let root = tempfile::tempdir().unwrap();
        let host = Host::build(&write_test_profile(root.path())).await.unwrap();
        let project: dwo_project::Project = serde_json::from_value(
            host.handle_method(
                "project.create",
                json!({"name": "Demo", "kind": "shared", "pwd": root.path()}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let job = json!({
            "name": "daily-report",
            "enabled": true,
            "schedule": {"cron": "0 9 * * *", "timezone": "Asia/Shanghai"},
            "session": {"mode": "new", "behavior": "every_time"},
            "prompt": "summarize the project"
        });

        host.handle_method(
            "automation.add",
            json!({"project_id": project.id, "job": job}),
        )
        .await
        .unwrap();
        assert_eq!(host.automation.list(Some(&project.id)).await.len(), 1);

        host.handle_method(
            "automation.disable",
            json!({"project_id": project.id, "job": "daily-report", "all": false}),
        )
        .await
        .unwrap();
        assert!(
            !host
                .automation
                .status(&project.id, "daily-report")
                .await
                .unwrap()
                .job
                .enabled
        );

        host.handle_method(
            "automation.delete",
            json!({"project_id": project.id, "job": "daily-report", "all": false}),
        )
        .await
        .unwrap();
        assert!(host.automation.list(Some(&project.id)).await.is_empty());
        assert!(
            host.projects
                .automation_config_path(&project.id)
                .unwrap()
                .is_file()
        );
        assert!(
            host.projects
                .automation_history_path(&project.id)
                .unwrap()
                .is_file()
        );
        assert!(!root.path().join("runtime/automation-runs.yaml").exists());
        host.shutdown().await;
    }

    #[tokio::test]
    async fn automation_run_returns_after_the_run_is_queued() {
        let profile = tempfile::tempdir().unwrap();
        let config = write_test_profile(profile.path());
        let host = Host::build(&config).await.unwrap();
        let project: dwo_project::Project = serde_json::from_value(
            host.handle_method(
                "project.create",
                json!({"name": "Demo", "kind": "shared", "pwd": profile.path()}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        for (name, session) in [
            (
                "background-failure",
                json!({"mode": "fixed", "sessionId": "invalid-session-id"}),
            ),
            (
                "valid-start",
                json!({"mode": "new", "behavior": "every_time"}),
            ),
        ] {
            host.handle_method(
                "automation.add",
                json!({
                    "project_id": project.id,
                    "job": {
                        "name": name,
                        "enabled": true,
                        "schedule": {"cron": "0 9 * * *", "timezone": "Asia/Shanghai"},
                        "session": session,
                        "prompt": "run this job"
                    }
                }),
            )
            .await
            .unwrap();
        }

        let error = host
            .handle_method(
                "automation.run",
                json!({"project_id": project.id, "job": "background-failure", "caller_session_id": null}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid") || error.to_string().contains("session"));

        let value = host
            .handle_method(
                "automation.run",
                json!({"project_id": project.id, "job": "valid-start", "caller_session_id": null}),
            )
            .await
            .unwrap();
        let record: crate::automation::AutomationRunRecord = serde_json::from_value(value).unwrap();
        assert_eq!(
            record.status,
            crate::automation::AutomationRunStatus::Queued
        );
        assert!(record.run_id.starts_with("run-"));
        assert!(record.session_id.is_some());
        assert!(record.turn_id.is_none());

        host.shutdown().await;
    }
}
