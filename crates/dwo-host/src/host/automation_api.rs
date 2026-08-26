use anyhow::{Context, Result};
use dwo_tools::SessionMode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use super::Host;
use crate::automation::{AutomationConfig, AutomationJob, parse_config as parse_automation_config};

#[derive(Deserialize)]
pub(crate) struct JobParam {
    pub(crate) job: String,
}

#[derive(Deserialize)]
pub(crate) struct AddParam {
    pub(crate) job: AutomationJob,
}

#[derive(Deserialize)]
pub(crate) struct ToggleParam {
    pub(crate) job: Option<String>,
    #[serde(default)]
    pub(crate) all: bool,
}

#[derive(Deserialize)]
pub(crate) struct UpdateParam {
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
    #[serde(default)]
    pub(crate) job: Option<String>,
    #[serde(default = "default_history_limit")]
    pub(crate) limit: usize,
}

#[derive(Deserialize)]
pub(crate) struct RunParam {
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
            "automation.list" => self.automation_list().await,
            "automation.status" => {
                let params: JobParam = serde_json::from_value(params)?;
                self.automation_status(params.job).await
            }
            "automation.update" => {
                let params: UpdateParam = serde_json::from_value(params)?;
                self.automation_update(params).await
            }
            "automation.history" => {
                let params: HistoryParam = serde_json::from_value(params)?;
                self.automation_history(params.job, params.limit).await
            }
            "automation.add" => {
                let params: AddParam = serde_json::from_value(params)?;
                self.automation_add(params.job).await
            }
            "automation.enable" | "automation.disable" => {
                let params: ToggleParam = serde_json::from_value(params)?;
                self.automation_set_enabled(params.job, params.all, method == "automation.enable")
                    .await
            }
            "automation.delete" => {
                let params: ToggleParam = serde_json::from_value(params)?;
                self.automation_delete(params.job, params.all).await
            }
            "automation.run" => {
                let params: RunParam = serde_json::from_value(params)?;
                self.automation_run(params.job, params.caller_session_id)
                    .await
            }
            other => anyhow::bail!("unknown automation method: {other}"),
        }
    }

    pub(crate) async fn automation_list(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.automation.list().await)?)
    }

    pub(crate) async fn automation_status(&self, job: String) -> Result<Value> {
        Ok(serde_json::to_value(self.automation.status(&job).await?)?)
    }

    pub(crate) async fn automation_update(self: &Arc<Self>, params: UpdateParam) -> Result<Value> {
        let name = params.name.clone();
        self.mutate_automation_config(|config| {
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
                json!({"job": name, "action": "update"}),
            )
            .await;
        Ok(serde_json::to_value(self.automation.status(&name).await?)?)
    }

    pub(crate) async fn automation_history(
        &self,
        job: Option<String>,
        limit: usize,
    ) -> Result<Value> {
        Ok(json!({"runs": self.automation.history(job.as_deref(), limit).await}))
    }

    pub(crate) async fn automation_add(self: &Arc<Self>, job: AutomationJob) -> Result<Value> {
        let name = job.name.clone();
        self.mutate_automation_config(|config| {
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
            .publish("automation.changed", json!({"job": name, "action": "add"}))
            .await;
        Ok(serde_json::to_value(self.automation.status(&name).await?)?)
    }

    pub(crate) async fn automation_set_enabled(
        self: &Arc<Self>,
        job: Option<String>,
        all: bool,
        enabled: bool,
    ) -> Result<Value> {
        anyhow::ensure!(all ^ job.is_some(), "specify a job or --all");
        let event_job = job.clone();
        self.mutate_automation_config(|config| {
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
                json!({"job": event_job, "all": all, "action": if enabled { "enable" } else { "disable" }}),
            )
            .await;
        Ok(json!({
            "updated": if all { "all" } else { job.as_deref().unwrap_or_default() },
            "enabled": enabled
        }))
    }

    pub(crate) async fn automation_delete(
        self: &Arc<Self>,
        job: Option<String>,
        all: bool,
    ) -> Result<Value> {
        anyhow::ensure!(all ^ job.is_some(), "specify a job or --all");
        let event_job = job.clone();
        let removed_jobs = if all {
            self.automation
                .list()
                .await
                .into_iter()
                .map(|status| status.job.name)
                .collect::<Vec<_>>()
        } else {
            job.iter().cloned().collect::<Vec<_>>()
        };
        self.mutate_automation_config(|config| {
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
            .remove_job_state(job.as_deref(), all)
            .await?;
        for job in &removed_jobs {
            self.projects.unassign_task_everywhere(job)?;
        }
        self.events
            .publish(
                "automation.changed",
                json!({"job": event_job, "all": all, "action": "delete"}),
            )
            .await;
        Ok(json!({"deleted": if all { "all" } else { job.as_deref().unwrap_or_default() }}))
    }

    pub(crate) async fn automation_run(
        self: &Arc<Self>,
        job: String,
        caller_session_id: Option<String>,
    ) -> Result<Value> {
        let caller = super::session_api::parse_optional_session(caller_session_id)?;
        if let Some(caller) = &caller {
            self.service.load(caller).await?;
        }
        let run = self.automation.run_now(&job, caller).await?;
        self.events
            .publish("automation.run", json!({"job": job, "runId": run.run_id}))
            .await;
        Ok(serde_json::to_value(run)?)
    }

    async fn mutate_automation_config<F>(self: &Arc<Self>, update: F) -> Result<()>
    where
        F: FnOnce(&mut AutomationConfig) -> Result<()>,
    {
        self.edit_profile(|profile| {
            let mut automation = parse_automation_config(profile.automation.clone())?;
            update(&mut automation)?;
            parse_automation_config(serde_yaml::to_value(&automation)?)?;
            profile.automation = serde_yaml::to_value(automation)?;
            Ok(())
        })
        .await?;
        Ok(())
    }
}
