use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use dwo_agent_service::{ExternalRuleFile, SessionId};
use dwo_project::CreateProject;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Host;
use crate::automation::AutomationJob;

#[derive(Deserialize)]
struct ProjectIdParam {
    project_id: String,
}

#[derive(Deserialize)]
struct CreateProjectParam {
    name: String,
    pwd: Option<PathBuf>,
}

#[derive(Deserialize)]
struct UpdateProjectParam {
    project_id: String,
    name: String,
}

#[derive(Deserialize)]
struct SectionParam {
    project_id: String,
    section_id: String,
}

#[derive(Deserialize)]
struct CreateSectionParam {
    project_id: String,
    name: String,
}

#[derive(Deserialize)]
struct UpdateSectionParam {
    project_id: String,
    section_id: String,
    name: String,
}

#[derive(Deserialize)]
struct ReorderSectionParam {
    project_id: String,
    section_id: String,
    position: usize,
}

#[derive(Deserialize)]
struct TopicParam {
    project_id: String,
    topic_id: String,
}

#[derive(Deserialize)]
struct CreateTopicParam {
    project_id: String,
    section_id: String,
    title: String,
}

#[derive(Deserialize)]
struct UpdateTopicParam {
    project_id: String,
    topic_id: String,
    title: String,
}

#[derive(Deserialize)]
struct MoveTopicParam {
    project_id: String,
    topic_id: String,
    section_id: String,
    position: usize,
}

#[derive(Deserialize)]
struct MarkdownParam {
    project_id: String,
    topic_id: String,
    content: String,
}

#[derive(Deserialize)]
struct TopicSessionParam {
    project_id: String,
    topic_id: String,
    session_id: String,
}

#[derive(Deserialize)]
struct TopicTaskParam {
    project_id: String,
    topic_id: String,
    task_id: String,
}

#[derive(Deserialize)]
struct CreateTopicTaskParam {
    project_id: String,
    topic_id: String,
    job: AutomationJob,
}

#[derive(Deserialize)]
struct LabelParam {
    project_id: String,
    label_id: String,
}

#[derive(Deserialize)]
struct CreateLabelParam {
    project_id: String,
    name: String,
    color: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct UpdateLabelParam {
    project_id: String,
    label_id: String,
    name: String,
    color: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct TopicLabelParam {
    project_id: String,
    topic_id: String,
    label_id: String,
}

impl Host {
    pub(crate) async fn dispatch_project(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let result = match method {
            "project.list" => serde_json::to_value(self.projects.list())?,
            "project.get" | "project.board" => {
                let params: ProjectIdParam = serde_json::from_value(params)?;
                serde_json::to_value(self.projects.get(&params.project_id)?)?
            }
            "project.create" => {
                let params: CreateProjectParam = serde_json::from_value(params)?;
                let pwd = params.pwd.map(|pwd| {
                    if pwd.is_absolute() {
                        pwd
                    } else {
                        self.profile_root.join(pwd)
                    }
                });
                let project = self.projects.create(CreateProject {
                    name: params.name,
                    pwd,
                })?;
                self.project_changed(&project.id, "create").await;
                serde_json::to_value(project)?
            }
            "project.update" => {
                let params: UpdateProjectParam = serde_json::from_value(params)?;
                let project = self
                    .projects
                    .update_project(&params.project_id, params.name)?;
                self.project_changed(&params.project_id, "update").await;
                serde_json::to_value(project)?
            }
            "project.section.create" => {
                let params: CreateSectionParam = serde_json::from_value(params)?;
                let section = self
                    .projects
                    .create_section(&params.project_id, params.name)?;
                self.project_changed(&params.project_id, "section.create")
                    .await;
                serde_json::to_value(section)?
            }
            "project.section.update" => {
                let params: UpdateSectionParam = serde_json::from_value(params)?;
                let section = self.projects.update_section(
                    &params.project_id,
                    &params.section_id,
                    params.name,
                )?;
                self.project_changed(&params.project_id, "section.update")
                    .await;
                serde_json::to_value(section)?
            }
            "project.section.delete" => {
                let params: SectionParam = serde_json::from_value(params)?;
                let project = self
                    .projects
                    .delete_section(&params.project_id, &params.section_id)?;
                self.project_changed(&params.project_id, "section.delete")
                    .await;
                serde_json::to_value(project)?
            }
            "project.section.reorder" => {
                let params: ReorderSectionParam = serde_json::from_value(params)?;
                let sections = self.projects.reorder_section(
                    &params.project_id,
                    &params.section_id,
                    params.position,
                )?;
                self.project_changed(&params.project_id, "section.reorder")
                    .await;
                serde_json::to_value(sections)?
            }
            "project.topic.get" => {
                let params: TopicParam = serde_json::from_value(params)?;
                self.project_topic_detail(&params.project_id, &params.topic_id)
                    .await?
            }
            "project.topic.create" => {
                let params: CreateTopicParam = serde_json::from_value(params)?;
                let topic = self.projects.create_topic(
                    &params.project_id,
                    &params.section_id,
                    params.title,
                )?;
                self.project_changed(&params.project_id, "topic.create")
                    .await;
                serde_json::to_value(topic)?
            }
            "project.topic.update" => {
                let params: UpdateTopicParam = serde_json::from_value(params)?;
                let topic = self.projects.update_topic(
                    &params.project_id,
                    &params.topic_id,
                    params.title,
                )?;
                self.project_changed(&params.project_id, "topic.update")
                    .await;
                serde_json::to_value(topic)?
            }
            "project.topic.move" | "project.topic.reorder" => {
                let params: MoveTopicParam = serde_json::from_value(params)?;
                let topic = self.projects.move_topic(
                    &params.project_id,
                    &params.topic_id,
                    &params.section_id,
                    params.position,
                )?;
                self.project_changed(&params.project_id, "topic.move").await;
                serde_json::to_value(topic)?
            }
            "project.topic.delete" => {
                let params: TopicParam = serde_json::from_value(params)?;
                self.move_topic_sessions_to_uncategorized(&params.project_id, &params.topic_id)
                    .await?;
                let project = self
                    .projects
                    .delete_topic(&params.project_id, &params.topic_id)?;
                self.project_changed(&params.project_id, "topic.delete")
                    .await;
                serde_json::to_value(project)?
            }
            "project.topic.overview.get" => {
                let params: TopicParam = serde_json::from_value(params)?;
                json!({"content": self.projects.overview(&params.project_id, &params.topic_id)?})
            }
            "project.topic.overview.set" => {
                let params: MarkdownParam = serde_json::from_value(params)?;
                self.projects.set_overview(
                    &params.project_id,
                    &params.topic_id,
                    &params.content,
                )?;
                self.project_changed(&params.project_id, "topic.overview.set")
                    .await;
                json!({"updated": true})
            }
            "project.topic.agents.get" => {
                let params: TopicParam = serde_json::from_value(params)?;
                json!({"content": self.projects.agents(&params.project_id, &params.topic_id)?})
            }
            "project.topic.agents.set" => {
                let params: MarkdownParam = serde_json::from_value(params)?;
                self.projects
                    .set_agents(&params.project_id, &params.topic_id, &params.content)?;
                self.project_changed(&params.project_id, "topic.agents.set")
                    .await;
                json!({"updated": true})
            }
            "project.topic.session.assign" => {
                let params: TopicSessionParam = serde_json::from_value(params)?;
                let topic = self
                    .assign_session_to_topic(
                        &params.project_id,
                        &params.topic_id,
                        &params.session_id,
                    )
                    .await?;
                self.project_changed(&params.project_id, "topic.session.assign")
                    .await;
                serde_json::to_value(topic)?
            }
            "project.topic.session.unassign" => {
                let params: TopicSessionParam = serde_json::from_value(params)?;
                let project = self.projects.get(&params.project_id)?;
                let topic = self
                    .assign_session_to_topic(
                        &params.project_id,
                        &project.board.uncategorized_topic_id,
                        &params.session_id,
                    )
                    .await?;
                self.project_changed(&params.project_id, "topic.session.unassign")
                    .await;
                serde_json::to_value(topic)?
            }
            "project.topic.task.assign" => {
                let params: TopicTaskParam = serde_json::from_value(params)?;
                self.automation.status(&params.task_id).await?;
                let topic = self.projects.assign_task(
                    &params.project_id,
                    &params.topic_id,
                    params.task_id,
                )?;
                self.project_changed(&params.project_id, "topic.task.assign")
                    .await;
                serde_json::to_value(topic)?
            }
            "project.topic.task.unassign" => {
                let params: TopicTaskParam = serde_json::from_value(params)?;
                let project = self.projects.get(&params.project_id)?;
                let topic = self.projects.assign_task(
                    &params.project_id,
                    &project.board.uncategorized_topic_id,
                    params.task_id,
                )?;
                self.project_changed(&params.project_id, "topic.task.unassign")
                    .await;
                serde_json::to_value(topic)?
            }
            "project.topic.task.create" => {
                let params: CreateTopicTaskParam = serde_json::from_value(params)?;
                let task_id = params.job.name.clone();
                self.automation_add(params.job).await?;
                let topic = self.projects.assign_task(
                    &params.project_id,
                    &params.topic_id,
                    task_id.clone(),
                )?;
                self.project_changed(&params.project_id, "topic.task.create")
                    .await;
                json!({"task": self.automation.status(&task_id).await?, "topic": topic})
            }
            "project.label.create" => {
                let params: CreateLabelParam = serde_json::from_value(params)?;
                let label = self.projects.create_label(
                    &params.project_id,
                    params.name,
                    params.color,
                    params.description,
                )?;
                self.project_changed(&params.project_id, "label.create")
                    .await;
                serde_json::to_value(label)?
            }
            "project.label.update" => {
                let params: UpdateLabelParam = serde_json::from_value(params)?;
                let label = self.projects.update_label(
                    &params.project_id,
                    &params.label_id,
                    params.name,
                    params.color,
                    params.description,
                )?;
                self.project_changed(&params.project_id, "label.update")
                    .await;
                serde_json::to_value(label)?
            }
            "project.label.delete" => {
                let params: LabelParam = serde_json::from_value(params)?;
                let project = self
                    .projects
                    .delete_label(&params.project_id, &params.label_id)?;
                self.project_changed(&params.project_id, "label.delete")
                    .await;
                serde_json::to_value(project)?
            }
            "project.label.assign" => {
                let params: TopicLabelParam = serde_json::from_value(params)?;
                let topic = self.projects.assign_label(
                    &params.project_id,
                    &params.topic_id,
                    &params.label_id,
                )?;
                self.project_changed(&params.project_id, "label.assign")
                    .await;
                serde_json::to_value(topic)?
            }
            "project.label.unassign" => {
                let params: TopicLabelParam = serde_json::from_value(params)?;
                let topic = self.projects.unassign_label(
                    &params.project_id,
                    &params.topic_id,
                    &params.label_id,
                )?;
                self.project_changed(&params.project_id, "label.unassign")
                    .await;
                serde_json::to_value(topic)?
            }
            _ => anyhow::bail!("unknown project method: {method}"),
        };
        Ok(result)
    }

    async fn project_topic_detail(&self, project_id: &str, topic_id: &str) -> Result<Value> {
        let project = self.projects.get(project_id)?;
        let topic = project
            .board
            .topics
            .iter()
            .find(|topic| topic.id == topic_id)
            .cloned()
            .with_context(|| format!("topic not found: {topic_id}"))?;
        let labels = project
            .board
            .labels
            .iter()
            .filter(|label| topic.label_ids.contains(&label.id))
            .cloned()
            .collect::<Vec<_>>();
        let mut sessions = Vec::new();
        for id in &topic.session_ids {
            let id = SessionId::parse(id.clone()).map_err(anyhow::Error::msg)?;
            if let Ok(status) = self.service.status(&id).await {
                sessions.push(status);
            }
        }
        let mut tasks = Vec::new();
        for id in &topic.task_ids {
            if let Ok(status) = self.automation.status(id).await {
                tasks.push(status);
            }
        }
        Ok(json!({
            "topic": topic,
            "overview": self.projects.overview(project_id, topic_id)?,
            "agents": self.projects.agents(project_id, topic_id)?,
            "labels": labels,
            "sessions": sessions,
            "tasks": tasks,
        }))
    }

    async fn assign_session_to_topic(
        &self,
        project_id: &str,
        topic_id: &str,
        session_id: &str,
    ) -> Result<dwo_project::Topic> {
        let project = self.projects.get(project_id)?;
        let agents_path = self.projects.agents_path(project_id, topic_id)?;
        let session_id = SessionId::parse(session_id.to_string()).map_err(anyhow::Error::msg)?;
        let snapshot = self.service.snapshot(&session_id).await?;
        anyhow::ensure!(
            snapshot.record.info.cwd == project.pwd,
            "session cwd does not match project pwd"
        );
        self.service.set_external_rule_files(
            &session_id,
            vec![ExternalRuleFile::new(agents_path, project.pwd)],
        );
        Ok(self
            .projects
            .assign_session(project_id, topic_id, session_id.to_string())?)
    }

    async fn move_topic_sessions_to_uncategorized(
        &self,
        project_id: &str,
        topic_id: &str,
    ) -> Result<()> {
        let project = self.projects.get(project_id)?;
        let Some(topic) = project
            .board
            .topics
            .iter()
            .find(|topic| topic.id == topic_id)
        else {
            anyhow::bail!("topic not found: {topic_id}");
        };
        if topic.id == project.board.uncategorized_topic_id {
            anyhow::bail!("the uncategorized topic cannot be deleted");
        }
        for session_id in topic.session_ids.clone() {
            self.assign_session_to_topic(
                project_id,
                &project.board.uncategorized_topic_id,
                &session_id,
            )
            .await?;
        }
        Ok(())
    }

    async fn project_changed(&self, project_id: &str, action: &str) {
        self.events
            .publish(
                "project.changed",
                json!({"projectId": project_id, "action": action}),
            )
            .await;
    }
}
