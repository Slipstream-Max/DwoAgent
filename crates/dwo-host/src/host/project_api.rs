use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use dwo_agent_service::SessionId;
use dwo_project::{
    CreateProject, Project, ProjectKind, RepositoryRecord, WorktreeRecord, WorktreeSource,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Host;

#[derive(Deserialize)]
struct ProjectIdParam {
    project_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProjectParam {
    name: Option<String>,
    pwd: PathBuf,
}

#[derive(Deserialize)]
struct ArchiveParam {
    project_id: String,
    section_id: Option<String>,
    topic_id: Option<String>,
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct ProjectRuleParam {
    project_id: String,
    content: Option<String>,
}

#[derive(Deserialize)]
struct UpdateProjectParam {
    project_id: String,
    name: String,
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
    path: Option<PathBuf>,
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
    overview: String,
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
                anyhow::ensure!(params.pwd.is_absolute(), "project pwd must be absolute");
                let name = params
                    .name
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| {
                        params
                            .pwd
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                    .context("project name is required for a filesystem root")?;
                let info = super::git::inspect_repository(&params.pwd).await.ok();
                let mut project = self.projects.create(CreateProject {
                    name,
                    kind: ProjectKind::Project,
                    pwd: Some(params.pwd),
                })?;
                if let Some(info) = info {
                    project = self.register_repository(&project.id, info, "Local")?;
                }
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
                self.worktree_views(&params.project_id)
                    .await?
                    .into_iter()
                    .find(|view| view["worktree"]["id"] == params.worktree_id)
                    .with_context(|| format!("worktree not found: {}", params.worktree_id))?
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
                let name = params.name.unwrap_or_else(|| params.branch.clone());
                anyhow::ensure!(
                    !name.trim().is_empty()
                        && !name.contains(['/', '\\', ':'])
                        && name != "."
                        && name != "..",
                    "worktree name must be a single directory name"
                );
                let path = project
                    .pwd
                    .as_ref()
                    .and_then(|p| p.parent())
                    .context("project path has no parent")?
                    .join(&name);
                anyhow::ensure!(
                    params.path.as_ref().is_none_or(|p| *p == path),
                    "worktree path must be a sibling of project pwd named after the worktree"
                );
                anyhow::ensure!(!path.exists(), "worktree path already exists");
                let status = super::git::create_worktree(
                    &repository.root,
                    &path,
                    &params.branch,
                    params.start_point.as_deref(),
                )
                .await?;
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
            "project.worktree.detach" | "project.worktree.remove" => {
                let params: WorktreeParam = serde_json::from_value(params)?;
                let project = self.projects.get(&params.project_id)?;
                anyhow::ensure!(
                    find_worktree(&project, &params.worktree_id)?.source != WorktreeSource::Primary,
                    "the primary worktree cannot be detached"
                );
                let project = self
                    .projects
                    .remove_worktree(&params.project_id, &params.worktree_id)?;
                self.project_changed(&params.project_id, "worktree.detach")
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
            "project.archive"
            | "project.section.archive"
            | "project.topic.archive"
            | "project.session.archive" => {
                let params: ArchiveParam = serde_json::from_value(params)?;
                anyhow::ensure!(
                    (method == "project.archive"
                        && params.section_id.is_none()
                        && params.topic_id.is_none()
                        && params.session_id.is_none())
                        || (method == "project.section.archive"
                            && params.section_id.is_some()
                            && params.topic_id.is_none()
                            && params.session_id.is_none())
                        || (method == "project.topic.archive"
                            && params.topic_id.is_some()
                            && params.section_id.is_none()
                            && params.session_id.is_none())
                        || (method == "project.session.archive"
                            && params.session_id.is_some()
                            && params.section_id.is_none()
                            && params.topic_id.is_none()),
                    "archive requires exactly the target ID for this operation"
                );
                let ids = self.archive_container(params).await?;
                json!({"archived": ids})
            }
            "project.agents.get" | "project.agents.set" => {
                let params: ProjectRuleParam = serde_json::from_value(params)?;
                if method.ends_with(".set") {
                    self.projects.set_project_rule(
                        &params.project_id,
                        params.content.as_deref().context("content is required")?,
                    )?;
                    self.project_changed(&params.project_id, "agents.set").await;
                }
                json!({"content": std::fs::read_to_string(self.projects.project_rule_path(&params.project_id)?).unwrap_or_default()})
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
                anyhow::ensure!(
                    !params.overview.trim().is_empty(),
                    "topic overview is required"
                );
                let topic = self.projects.create_topic(
                    &params.project_id,
                    &params.section_id,
                    params.title,
                )?;
                self.projects
                    .set_overview(&params.project_id, &topic.id, &params.overview)?;
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
                        && snapshot.record.info.cwd == worktree.path
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

    async fn archive_container(&self, params: ArchiveParam) -> Result<Vec<String>> {
        let _lifecycle = self.automation.lifecycle.lock().await;
        let project = self.projects.get(&params.project_id)?;
        let topics: Vec<String> = project
            .board
            .topics
            .iter()
            .filter(|t| {
                params
                    .section_id
                    .as_ref()
                    .is_none_or(|id| &t.section_id == id)
                    && params.topic_id.as_ref().is_none_or(|id| &t.id == id)
            })
            .map(|t| t.id.clone())
            .collect();
        let ids: Vec<String> = project
            .board
            .topics
            .iter()
            .filter(|t| topics.contains(&t.id))
            .flat_map(|t| t.session_ids.iter())
            .filter(|id| params.session_id.as_ref().is_none_or(|s| *id == s))
            .cloned()
            .collect();
        for value in &ids {
            let id = SessionId::parse(value.clone()).map_err(anyhow::Error::msg)?;
            let snapshot = self.service.snapshot(&id).await?;
            anyhow::ensure!(
                snapshot.phase == dwo_agent_service::RuntimePhase::Idle,
                "stop session {id} before archiving"
            );
        }
        let jobs = self.automation.list(Some(&project.id)).await;
        anyhow::ensure!(
            jobs.iter().all(|j| j.active_runs.is_empty()),
            "stop project automation runs before archiving"
        );
        self.automation.update_project_config(&project.id, |config| {
                for job in &mut config.jobs {
                    let fixed_selected = matches!(&job.session, crate::automation::AutomationSession::Fixed { session_id } if ids.contains(session_id));
                    if fixed_selected || (params.session_id.is_none() && topics.contains(&job.topic_id.clone().unwrap_or_else(|| project.board.uncategorized_topic_id.clone()))) {
                        job.enabled = false;
                    }
                }
                Ok(())
            }).await?;
        let ids = self.projects.archive(
            &project.id,
            params.section_id.as_deref(),
            params.topic_id.as_deref(),
            params.session_id.as_deref(),
        )?;
        for value in &ids {
            let id = SessionId::parse(value.clone()).map_err(anyhow::Error::msg)?;
            self.service.set_external_rule_files(&id, vec![]);
        }
        self.project_changed(&project.id, "archive").await;
        self.project_changed(dwo_project::UNASSIGNED_PROJECT_ID, "archive")
            .await;
        Ok(ids)
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
    async fn project_archive_preserves_cwd_and_requires_archive_for_deletion() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("demo");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("keep.txt"), "keep").unwrap();
        let host = Host::build(&write_test_profile(root.path())).await.unwrap();
        let project = host
            .handle_method("project.create", json!({"pwd": workspace}))
            .await
            .unwrap();
        assert_eq!(project["name"], "demo");
        assert_eq!(project["kind"], "project");
        let pid = project["id"].as_str().unwrap();
        let section = project["board"]["uncategorizedSectionId"].as_str().unwrap();
        assert!(
            host.handle_method(
                "project.topic.create",
                json!({"project_id": pid, "section_id": section, "title": "Task", "overview": ""})
            )
            .await
            .is_err()
        );
        let topic = host.handle_method("project.topic.create", json!({"project_id": pid, "section_id": section, "title": "Task", "overview": "Implement feature"})).await.unwrap();
        let tid = topic["id"].as_str().unwrap();
        host.handle_method(
            "project.agents.set",
            json!({"project_id": pid, "content": "Project rule sentinel"}),
        )
        .await
        .unwrap();
        host.handle_method(
            "project.topic.agents.set",
            json!({"project_id": pid, "topic_id": tid, "content": "Topic rule sentinel"}),
        )
        .await
        .unwrap();
        let created = host
            .handle_method("session.new", json!({"project_id": pid, "topic_id": tid}))
            .await
            .unwrap();
        let id = SessionId::parse(created["session_id"].as_str().unwrap()).unwrap();
        let snapshot = host.service.snapshot(&id).await.unwrap();
        let cwd = snapshot.record.info.cwd;
        let prompt = serde_json::to_string(&snapshot.record.context).unwrap();
        assert!(prompt.contains("Project rule sentinel"));
        assert!(prompt.contains("Topic rule sentinel"));
        assert!(host.delete_session(&id).await.is_err());
        assert!(
            host.handle_method(
                "session.set",
                json!({"session_id": id, "worktree_id": "other"})
            )
            .await
            .is_err()
        );
        host.handle_method("project.archive", json!({"project_id": pid}))
            .await
            .unwrap();
        assert!(host.projects.is_archived(id.as_str()));
        host.service.unload(&id).await.unwrap();
        assert_eq!(
            host.service.snapshot(&id).await.unwrap().record.info.cwd,
            cwd
        );
        assert!(host.projects.get(pid).is_err());
        assert!(
            host.prompt_session(
                &id,
                dwo_agent_service::EndpointId::parse("test").unwrap(),
                dwo_context::MessageContent::text("run")
            )
            .await
            .is_err()
        );
        host.delete_session(&id).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("keep.txt")).unwrap(),
            "keep"
        );
        host.shutdown().await;
    }

    #[tokio::test]
    async fn worktree_is_a_sibling_and_detach_preserves_checkout_and_session() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(&repo)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let host = Host::build(&write_test_profile(root.path())).await.unwrap();
        let project = host
            .handle_method("project.create", json!({"pwd": repo}))
            .await
            .unwrap();
        let pid = project["id"].as_str().unwrap();
        assert!(!project["repository"].is_null());
        let project = host
            .handle_method(
                "project.worktree.create",
                json!({"project_id": pid, "branch": "feature-a", "name": "feature-a"}),
            )
            .await
            .unwrap();
        let tree = project["worktrees"].as_array().unwrap().last().unwrap();
        let wid = tree["id"].as_str().unwrap();
        let cwd = std::fs::canonicalize(root.path().join("feature-a")).unwrap();
        let created = host
            .handle_method(
                "session.new",
                json!({"project_id": pid, "worktree_id": wid}),
            )
            .await
            .unwrap();
        let id = SessionId::parse(created["session_id"].as_str().unwrap()).unwrap();
        host.handle_method(
            "project.worktree.remove",
            json!({"project_id": pid, "worktree_id": wid}),
        )
        .await
        .unwrap();
        host.service.unload(&id).await.unwrap();
        assert_eq!(
            host.service.snapshot(&id).await.unwrap().record.info.cwd,
            cwd
        );
        assert!(cwd.join(".git").exists());
        assert!(
            host.handle_method(
                "project.worktree.create",
                json!({"project_id": pid, "branch": "bad", "name": "../escape"})
            )
            .await
            .is_err()
        );
        assert!(
            host.handle_method(
                "project.worktree.create",
                json!({"project_id": pid, "branch": "other", "name": "feature-a"})
            )
            .await
            .is_err()
        );
        assert!(
            host.handle_method("project.repository.clone", json!({}))
                .await
                .is_err()
        );
        assert!(
            host.handle_method("project.topic.move_to_project", json!({}))
                .await
                .is_err()
        );
        host.shutdown().await;
    }
}
