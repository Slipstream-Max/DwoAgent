use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use dwo_agent_service::{ExternalRuleFile, SessionId};
use dwo_project::{CreateProject, Project, RepositoryRecord, WorktreeRecord, WorktreeSource};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Host;

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
struct CloneRepositoryParam {
    project_id: String,
    url: String,
    path: PathBuf,
    branch: Option<String>,
}

#[derive(Deserialize)]
struct AttachRepositoryParam {
    project_id: String,
    path: PathBuf,
    name: Option<String>,
}

#[derive(Deserialize)]
struct WorktreeParam {
    project_id: String,
    worktree_id: String,
}

#[derive(Deserialize)]
struct AttachWorktreeParam {
    project_id: String,
    path: PathBuf,
    name: Option<String>,
}

#[derive(Deserialize)]
struct CreateWorktreeParam {
    project_id: String,
    path: PathBuf,
    branch: String,
    start_point: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct UpdateWorktreeParam {
    project_id: String,
    worktree_id: String,
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
            "project.repository.get" => {
                let params: ProjectIdParam = serde_json::from_value(params)?;
                serde_json::to_value(self.projects.get(&params.project_id)?.repository)?
            }
            "project.repository.clone" => {
                let params: CloneRepositoryParam = serde_json::from_value(params)?;
                let info = super::git::clone_repository(
                    &params.url,
                    &self.profile_path(params.path),
                    params.branch.as_deref(),
                )
                .await?;
                let project = self.register_repository(&params.project_id, info, "Local")?;
                self.project_changed(&params.project_id, "repository.clone")
                    .await;
                serde_json::to_value(project)?
            }
            "project.repository.attach" => {
                let params: AttachRepositoryParam = serde_json::from_value(params)?;
                let info = super::git::inspect_repository(&self.profile_path(params.path)).await?;
                let project = self.register_repository(
                    &params.project_id,
                    info,
                    params.name.as_deref().unwrap_or("Local"),
                )?;
                self.project_changed(&params.project_id, "repository.attach")
                    .await;
                serde_json::to_value(project)?
            }
            "project.worktree.list" => {
                let params: ProjectIdParam = serde_json::from_value(params)?;
                serde_json::to_value(self.worktree_views(&params.project_id).await?)?
            }
            "project.worktree.get" => {
                let params: WorktreeParam = serde_json::from_value(params)?;
                let view = self
                    .worktree_views(&params.project_id)
                    .await?
                    .into_iter()
                    .find(|view| view["worktree"]["id"] == params.worktree_id)
                    .with_context(|| format!("worktree not found: {}", params.worktree_id))?;
                view
            }
            "project.worktree.attach" => {
                let params: AttachWorktreeParam = serde_json::from_value(params)?;
                let project = self.projects.get(&params.project_id)?;
                let repository = project
                    .repository
                    .as_ref()
                    .context("project has no attached repository")?;
                let info = super::git::inspect_repository(&self.profile_path(params.path)).await?;
                anyhow::ensure!(
                    info.common_dir == repository.common_dir,
                    "worktree belongs to a different Git repository"
                );
                let path = info.root;
                let name = params.name.unwrap_or_else(|| default_worktree_name(&path));
                let project = self.projects.add_worktree(
                    &params.project_id,
                    worktree_record(name, path, WorktreeSource::External),
                )?;
                self.project_changed(&params.project_id, "worktree.attach")
                    .await;
                serde_json::to_value(project)?
            }
            "project.worktree.create" => {
                let params: CreateWorktreeParam = serde_json::from_value(params)?;
                let project = self.projects.get(&params.project_id)?;
                let repository = project
                    .repository
                    .as_ref()
                    .context("project has no attached repository")?;
                let status = super::git::create_worktree(
                    &repository.root,
                    &self.profile_path(params.path),
                    &params.branch,
                    params.start_point.as_deref(),
                )
                .await?;
                let name = params.name.unwrap_or_else(|| params.branch.clone());
                let project = self.projects.add_worktree(
                    &params.project_id,
                    worktree_record(name, status.path, WorktreeSource::Managed),
                )?;
                self.project_changed(&params.project_id, "worktree.create")
                    .await;
                serde_json::to_value(project)?
            }
            "project.worktree.update" => {
                let params: UpdateWorktreeParam = serde_json::from_value(params)?;
                let worktree = self.projects.update_worktree(
                    &params.project_id,
                    &params.worktree_id,
                    params.name,
                )?;
                self.project_changed(&params.project_id, "worktree.update")
                    .await;
                serde_json::to_value(worktree)?
            }
            "project.worktree.detach" => {
                let params: WorktreeParam = serde_json::from_value(params)?;
                let project = self.projects.get(&params.project_id)?;
                anyhow::ensure!(
                    find_worktree(&project, &params.worktree_id)?.source != WorktreeSource::Primary,
                    "the primary worktree cannot be detached"
                );
                self.ensure_worktree_unused(&project, &params.worktree_id)
                    .await?;
                let project = self
                    .projects
                    .remove_worktree(&params.project_id, &params.worktree_id)?;
                self.project_changed(&params.project_id, "worktree.detach")
                    .await;
                serde_json::to_value(project)?
            }
            "project.worktree.remove" => {
                let params: WorktreeParam = serde_json::from_value(params)?;
                let project = self.projects.get(&params.project_id)?;
                let repository = project
                    .repository
                    .as_ref()
                    .context("project has no attached repository")?;
                let worktree = find_worktree(&project, &params.worktree_id)?;
                anyhow::ensure!(
                    worktree.source == WorktreeSource::Managed,
                    "only DWO-managed worktrees can be removed"
                );
                self.ensure_worktree_unused(&project, &params.worktree_id)
                    .await?;
                super::git::remove_worktree(&repository.root, &worktree.path).await?;
                let project = self
                    .projects
                    .remove_worktree(&params.project_id, &params.worktree_id)?;
                self.project_changed(&params.project_id, "worktree.remove")
                    .await;
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
                let project = self.projects.get(&params.project_id)?;
                self.move_topic_sessions_to_uncategorized(&params.project_id, &params.topic_id)
                    .await?;
                self.automation
                    .move_topic_jobs(
                        &params.project_id,
                        &params.topic_id,
                        &project.board.uncategorized_topic_id,
                    )
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
        let uncategorized_topic_id = project.board.uncategorized_topic_id.as_str();
        let tasks = self
            .automation
            .list(Some(project_id))
            .await
            .into_iter()
            .filter(|status| {
                status
                    .job
                    .topic_id
                    .as_deref()
                    .unwrap_or(uncategorized_topic_id)
                    == topic_id
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "topic": topic,
            "overview": self.projects.overview(project_id, topic_id)?,
            "agents": self.projects.agents(project_id, topic_id)?,
            "labels": labels,
            "sessions": sessions,
            "tasks": tasks,
        }))
    }

    fn profile_path(&self, path: PathBuf) -> PathBuf {
        if path.is_absolute() {
            path
        } else {
            self.profile_root.join(path)
        }
    }

    fn register_repository(
        &self,
        project_id: &str,
        info: super::git::RepositoryInfo,
        name: impl Into<String>,
    ) -> Result<Project> {
        anyhow::ensure!(
            self.projects.get(project_id)?.repository.is_none(),
            "project already has an attached repository"
        );
        self.projects
            .set_repository(
                project_id,
                RepositoryRecord {
                    root: info.root.clone(),
                    common_dir: info.common_dir,
                    remote_url: info.remote_url,
                },
                worktree_record(name.into(), info.root, WorktreeSource::Primary),
            )
            .map_err(Into::into)
    }

    async fn worktree_views(&self, project_id: &str) -> Result<Vec<Value>> {
        let project = self.projects.get(project_id)?;
        let mut views = Vec::with_capacity(project.worktrees.len());
        for worktree in project.worktrees {
            let status = super::git::worktree_status(&worktree.path).await;
            let mut topics = Vec::new();
            for topic in &project.board.topics {
                let mut sessions = Vec::new();
                for session_id in &topic.session_ids {
                    let id = SessionId::parse(session_id.clone()).map_err(anyhow::Error::msg)?;
                    if let Ok(snapshot) = self.service.status(&id).await
                        && snapshot.record.info.worktree_id.as_deref() == Some(worktree.id.as_str())
                    {
                        sessions.push(snapshot);
                    }
                }
                if !sessions.is_empty() {
                    topics.push(json!({"topic": topic, "sessions": sessions}));
                }
            }
            views.push(json!({
                "worktree": worktree,
                "git": status.as_ref().ok(),
                "available": status.is_ok(),
                "topics": topics,
            }));
        }
        Ok(views)
    }

    async fn ensure_worktree_unused(&self, project: &Project, worktree_id: &str) -> Result<()> {
        for session_id in project
            .board
            .topics
            .iter()
            .flat_map(|topic| &topic.session_ids)
        {
            let id = SessionId::parse(session_id.clone()).map_err(anyhow::Error::msg)?;
            if let Ok(snapshot) = self.service.snapshot(&id).await
                && snapshot.record.info.worktree_id.as_deref() == Some(worktree_id)
            {
                anyhow::bail!("worktree is still used by session {id}");
            }
        }
        Ok(())
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
        let cwd = match snapshot.record.info.worktree_id.as_deref() {
            Some(worktree_id) => find_worktree(&project, worktree_id)?.path.clone(),
            None => project.pwd.clone(),
        };
        anyhow::ensure!(
            snapshot.record.info.cwd == cwd,
            "session workspace is invalid"
        );
        self.service
            .set_external_rule_files(&session_id, vec![ExternalRuleFile::new(agents_path, cwd)]);
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

fn worktree_record(name: String, path: PathBuf, source: WorktreeSource) -> WorktreeRecord {
    WorktreeRecord {
        id: format!("worktree-{}", uuid::Uuid::new_v4()),
        name,
        path,
        source,
        created_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    }
}

pub(super) fn find_worktree<'a>(
    project: &'a Project,
    worktree_id: &str,
) -> Result<&'a WorktreeRecord> {
    project
        .worktrees
        .iter()
        .find(|worktree| worktree.id == worktree_id)
        .with_context(|| format!("worktree not found: {worktree_id}"))
}

fn default_worktree_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Worktree".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::tests::write_test_profile;

    #[tokio::test]
    async fn topic_sessions_inherit_the_project_workspace_and_rules() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let host = Host::build(&write_test_profile(root.path())).await.unwrap();

        let project = host
            .handle_method("project.create", json!({"name": "Demo", "pwd": workspace}))
            .await
            .unwrap();
        let project_id = project["id"].as_str().unwrap();
        let section_id = project["board"]["uncategorizedSectionId"].as_str().unwrap();
        let topic = host
            .handle_method(
                "project.topic.create",
                json!({
                    "project_id": project_id,
                    "section_id": section_id,
                    "title": "Project API"
                }),
            )
            .await
            .unwrap();
        let topic_id = topic["id"].as_str().unwrap();
        host.handle_method(
            "project.topic.agents.set",
            json!({
                "project_id": project_id,
                "topic_id": topic_id,
                "content": "Keep changes inside the project API."
            }),
        )
        .await
        .unwrap();

        let created = host
            .handle_method(
                "session.new",
                json!({"project_id": project_id, "topic_id": topic_id}),
            )
            .await
            .unwrap();
        let session_id =
            SessionId::parse(created["session_id"].as_str().unwrap().to_string()).unwrap();
        let snapshot = host.service.snapshot(&session_id).await.unwrap();
        assert_eq!(
            snapshot.record.info.cwd,
            std::fs::canonicalize(workspace).unwrap()
        );
        assert!(
            snapshot
                .record
                .context
                .system_prompt
                .content
                .contains("Keep changes inside the project API.")
        );
        let (_, assigned_topic) = host.projects.locate_session(session_id.as_str()).unwrap();
        assert_eq!(assigned_topic.id, topic_id);

        host.handle_method(
            "automation.add",
            json!({
                "project_id": project_id,
                "job": {
                    "name": "topic-review",
                    "enabled": true,
                    "schedule": {"cron": "0 9 * * *", "timezone": "Asia/Shanghai"},
                    "session": {"mode": "new", "behavior": "every_time"},
                    "topicId": topic_id,
                    "prompt": "Review now"
                }
            }),
        )
        .await
        .unwrap();
        let run = host
            .handle_method(
                "automation.run",
                json!({"project_id": project_id, "job": "topic-review", "caller_session_id": null}),
            )
            .await
            .unwrap();
        let automation_session_id = run["sessionId"].as_str().unwrap();
        let (_, automation_topic) = host
            .projects
            .locate_session(automation_session_id)
            .expect("topic automation session is assigned to its topic");
        assert_eq!(automation_topic.id, topic_id);
        let automation_snapshot = host
            .service
            .snapshot(&SessionId::parse(automation_session_id.to_string()).unwrap())
            .await
            .unwrap();
        assert!(
            automation_snapshot
                .record
                .context
                .system_prompt
                .content
                .contains("Keep changes inside the project API.")
        );

        host.shutdown().await;
    }

    #[tokio::test]
    async fn deleting_a_topic_moves_its_jobs_to_uncategorized() {
        let root = tempfile::tempdir().unwrap();
        let host = Host::build(&write_test_profile(root.path())).await.unwrap();
        let project = host
            .handle_method(
                "project.create",
                json!({"name": "Demo", "pwd": root.path()}),
            )
            .await
            .unwrap();
        let project_id = project["id"].as_str().unwrap();
        let uncategorized_topic_id = project["board"]["uncategorizedTopicId"].as_str().unwrap();
        let section_id = project["board"]["uncategorizedSectionId"].as_str().unwrap();
        let topic = host
            .handle_method(
                "project.topic.create",
                json!({"project_id": project_id, "section_id": section_id, "title": "Review"}),
            )
            .await
            .unwrap();
        let topic_id = topic["id"].as_str().unwrap();
        host.handle_method(
            "automation.add",
            json!({
                "project_id": project_id,
                "job": {
                    "name": "topic-review",
                    "schedule": {"cron": "0 9 * * *"},
                    "session": {"mode": "new", "behavior": "every_time"},
                    "topicId": topic_id,
                    "prompt": "Review now"
                }
            }),
        )
        .await
        .unwrap();

        host.handle_method(
            "project.topic.delete",
            json!({"project_id": project_id, "topic_id": topic_id}),
        )
        .await
        .unwrap();

        let status = host
            .automation
            .status(project_id, "topic-review")
            .await
            .unwrap();
        assert_eq!(status.job.topic_id.as_deref(), Some(uncategorized_topic_id));
        host.shutdown().await;
    }
}
