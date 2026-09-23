use anyhow::{Context, Result};
use dwo_tools::SessionMode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use super::Host;
use crate::automation::AutomationJob;

#[derive(Deserialize)]
struct AutomationRequest<T> {
    project_id: Option<String>,
    #[serde(default)]
    global: bool,
    caller_session_id: Option<String>,
    #[serde(flatten)]
    params: T,
}

impl<T> AutomationRequest<T> {
    async fn project(&self, host: &Host, custom_cwd: bool) -> Result<Option<String>> {
        let global = self.global || custom_cwd;
        anyhow::ensure!(
            !global || self.project_id.is_none(),
            "global/cwd cannot be combined with project_id"
        );
        if let Some(project_id) = &self.project_id {
            host.projects.get(project_id)?;
            return Ok(Some(project_id.clone()));
        }
        if global {
            return Ok(None);
        }
        let Some(caller) = &self.caller_session_id else {
            return Ok(None);
        };
        let id = dwo_agent_service::SessionId::parse(caller.clone()).map_err(anyhow::Error::msg)?;
        host.service.snapshot(&id).await?;
        Ok(host
            .projects
            .locate_session(caller)
            .map(|(project, _)| project.id))
    }
}

#[derive(Deserialize)]
struct ListParam {}

#[derive(Deserialize)]
struct JobParam {
    job: String,
}

#[derive(Deserialize)]
struct AddParam {
    job: AutomationJob,
}

#[derive(Deserialize)]
struct ToggleParam {
    job: Option<String>,
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct UpdateParam {
    name: String,
    job: Option<AutomationJob>,
    prompt: Option<String>,
    enabled: Option<bool>,
    model: Option<Option<String>>,
    reasoning: Option<Option<String>>,
    policy: Option<SessionMode>,
}

#[derive(Deserialize)]
struct HistoryParam {
    job: Option<String>,
    #[serde(default = "default_history_limit")]
    limit: usize,
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
                let request: AutomationRequest<ListParam> = serde_json::from_value(params)?;
                let project = request.project(self, false).await?;
                Ok(serde_json::to_value(self.automation.list(&project).await)?)
            }
            "automation.status" => {
                let request: AutomationRequest<JobParam> = serde_json::from_value(params)?;
                let project = request.project(self, false).await?;
                Ok(serde_json::to_value(
                    self.automation
                        .status(&project, &request.params.job)
                        .await?,
                )?)
            }
            "automation.update" => {
                let request: AutomationRequest<UpdateParam> = serde_json::from_value(params)?;
                let project = request
                    .project(
                        self,
                        request
                            .params
                            .job
                            .as_ref()
                            .is_some_and(|job| job.cwd.is_some()),
                    )
                    .await?;
                self.automation_update(project, request.params).await
            }
            "automation.history" => {
                let request: AutomationRequest<HistoryParam> = serde_json::from_value(params)?;
                let project = request.project(self, false).await?;
                Ok(
                    json!({"runs": self.automation.history(&project, request.params.job.as_deref(), request.params.limit).await}),
                )
            }
            "automation.add" => {
                let request: AutomationRequest<AddParam> = serde_json::from_value(params)?;
                let project = request
                    .project(self, request.params.job.cwd.is_some())
                    .await?;
                self.automation_add(project, request.params.job).await
            }
            "automation.enable" | "automation.disable" => {
                let request: AutomationRequest<ToggleParam> = serde_json::from_value(params)?;
                let project = request.project(self, false).await?;
                self.automation_set_enabled(
                    project,
                    request.params.job,
                    request.params.all,
                    method == "automation.enable",
                )
                .await
            }
            "automation.delete" => {
                let request: AutomationRequest<ToggleParam> = serde_json::from_value(params)?;
                let project = request.project(self, false).await?;
                self.automation_delete(project, request.params.job, request.params.all)
                    .await
            }
            "automation.run" => {
                let request: AutomationRequest<JobParam> = serde_json::from_value(params)?;
                let project = request.project(self, false).await?;
                self.automation_run(project, request.params.job, request.caller_session_id)
                    .await
            }
            other => anyhow::bail!("unknown automation method: {other}"),
        }
    }

    async fn automation_update(
        self: &Arc<Self>,
        project_id: Option<String>,
        params: UpdateParam,
    ) -> Result<Value> {
        let name = params.name.clone();
        self.automation
            .update_config(&project_id, |config| {
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

    async fn automation_add(
        self: &Arc<Self>,
        project_id: Option<String>,
        job: AutomationJob,
    ) -> Result<Value> {
        let name = job.name.clone();
        self.automation
            .update_config(&project_id, |config| {
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

    async fn automation_set_enabled(
        self: &Arc<Self>,
        project_id: Option<String>,
        job: Option<String>,
        all: bool,
        enabled: bool,
    ) -> Result<Value> {
        anyhow::ensure!(all ^ job.is_some(), "specify a job or --all");
        let event_job = job.clone();
        self.automation
            .update_config(&project_id, |config| {
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

    async fn automation_delete(
        self: &Arc<Self>,
        project_id: Option<String>,
        job: Option<String>,
        all: bool,
    ) -> Result<Value> {
        anyhow::ensure!(all ^ job.is_some(), "specify a job or --all");
        let event_job = job.clone();
        self.automation
            .update_config(&project_id, |config| {
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

    async fn automation_run(
        self: &Arc<Self>,
        project_id: Option<String>,
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
                json!({"name": "Demo", "pwd": root.path()}),
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
        assert_eq!(
            host.automation.list(&Some(project.id.clone())).await.len(),
            1
        );

        host.handle_method(
            "automation.disable",
            json!({"project_id": project.id, "job": "daily-report", "all": false}),
        )
        .await
        .unwrap();
        assert!(
            !host
                .automation
                .status(&Some(project.id.clone()), "daily-report")
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
        assert!(
            host.automation
                .list(&Some(project.id.clone()))
                .await
                .is_empty()
        );
        assert!(
            root.path()
                .join("runtime/automations")
                .join(&project.id)
                .join("config.yaml")
                .is_file()
        );
        assert!(
            root.path()
                .join("runtime/automations")
                .join(&project.id)
                .join("history.yaml")
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
                json!({"name": "Demo", "pwd": profile.path()}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        for (name, session) in [
            (
                "background-failure",
                json!({"mode": "fixed", "sessionId": "session-missing"}),
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
    #[tokio::test]
    async fn host_resolves_global_and_project_scope_and_deletes_bound_configs() {
        let root = tempfile::tempdir().unwrap();
        let config = write_test_profile(root.path());
        let host = Host::build(&config).await.unwrap();
        let unassigned = host.create_session(Default::default()).await.unwrap();
        let job = json!({"name": "same-name", "enabled": false,
            "schedule": {"cron": "0 9 * * *"}, "session": {"mode": "new", "behavior": "once"}, "prompt": "test"});
        let global = host
            .handle_method(
                "automation.add",
                json!({"caller_session_id": unassigned, "job": job}),
            )
            .await
            .unwrap();
        assert!(global["projectId"].is_null());
        let project = host
            .handle_method("project.create", json!({"pwd": root.path()}))
            .await
            .unwrap();
        let project_id = project["id"].as_str().unwrap();
        let assigned = host
            .create_session(super::super::HostSessionOptions {
                project_id: Some(project_id.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let scoped = host
            .handle_method(
                "automation.add",
                json!({"caller_session_id": assigned, "job": job}),
            )
            .await
            .unwrap();
        assert_eq!(scoped["projectId"], project_id);
        let listed = host
            .handle_method(
                "automation.list",
                json!({"caller_session_id": assigned, "global": true}),
            )
            .await
            .unwrap();
        assert!(listed[0]["projectId"].is_null());
        let mut custom = job;
        custom["name"] = json!("custom-path");
        custom["cwd"] = json!(root.path());
        let custom = host
            .handle_method(
                "automation.add",
                json!({"caller_session_id": assigned, "job": custom}),
            )
            .await
            .unwrap();
        assert!(custom["projectId"].is_null());
        let project_config = root.path().join("runtime/automations").join(project_id);
        assert!(project_config.join("config.yaml").is_file());
        host.handle_method("project.delete", json!({"project_id": project_id}))
            .await
            .unwrap();
        assert!(!project_config.exists());
        assert!(
            root.path()
                .join("runtime/automations/global/config.yaml")
                .is_file()
        );
        assert!(host.projects.locate_session(assigned.as_str()).is_none());
        host.shutdown().await;
        drop(host);
        let host = Host::build(&config).await.unwrap();
        assert_eq!(host.automation.list(&None).await.len(), 2);
        host.shutdown().await;
    }
}
