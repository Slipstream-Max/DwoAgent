use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use dwo_agent_service::SessionId;
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
    name: Option<String>,
    pwd: PathBuf,
}
#[derive(Deserialize)]
struct UpdateProjectParam {
    project_id: String,
    name: String,
}
#[derive(Deserialize)]
struct ProjectRuleParam {
    project_id: String,
    content: Option<String>,
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
    color: Option<String>,
}
#[derive(Deserialize)]
struct UpdateSectionParam {
    project_id: String,
    section_id: String,
    name: String,
    color: Option<String>,
}
#[derive(Deserialize)]
struct ReorderSectionParam {
    project_id: String,
    section_id: String,
    position: usize,
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
struct AssignSessionParam {
    project_id: String,
    section_id: Option<String>,
    session_id: String,
    worktree_id: Option<String>,
}

impl Host {
    pub(crate) async fn dispatch_project(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        match method {
            "project.list" => Ok(serde_json::to_value(self.projects.list())?),
            "project.get" => {
                let p: ProjectIdParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(self.projects.get(&p.project_id)?)?)
            }
            "project.create" => {
                let p: CreateProjectParam = serde_json::from_value(params)?;
                anyhow::ensure!(p.pwd.is_absolute(), "project pwd must be absolute");
                let mut project = self.projects.create(CreateProject {
                    name: p.name,
                    pwd: p.pwd.clone(),
                })?;
                if let Ok(info) = super::git::inspect_repository(&p.pwd).await {
                    project = self.register_repository(&project.id, info, "Local")?;
                }
                self.project_changed(&project.id, "create").await;
                Ok(serde_json::to_value(project)?)
            }
            "project.update" => {
                let p: UpdateProjectParam = serde_json::from_value(params)?;
                let project = self.projects.update_project(&p.project_id, p.name)?;
                self.project_changed(&p.project_id, "update").await;
                Ok(serde_json::to_value(project)?)
            }
            "project.delete" => {
                let p: ProjectIdParam = serde_json::from_value(params)?;
                self.automation.remove_project_config(&p.project_id).await?;
                let _lifecycle = self.sessions.lifecycle.lock().await;
                self.projects.delete(&p.project_id)?;
                self.project_changed(&p.project_id, "delete").await;
                Ok(json!({"deleted": true}))
            }
            "project.rules.get" => {
                let p: ProjectRuleParam = serde_json::from_value(params)?;
                Ok(json!({"content": self.projects.project_rule(&p.project_id)?}))
            }
            "project.rules.set" => {
                let p: ProjectRuleParam = serde_json::from_value(params)?;
                self.projects.set_project_rule(
                    &p.project_id,
                    p.content.as_deref().context("content is required")?,
                )?;
                self.project_changed(&p.project_id, "rules.set").await;
                Ok(json!({"updated": true}))
            }
            "project.section.list" => {
                let p: ProjectIdParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.projects.get(&p.project_id)?.sections,
                )?)
            }
            "project.section.create" => {
                let p: CreateSectionParam = serde_json::from_value(params)?;
                let section = self
                    .projects
                    .create_section(&p.project_id, p.name, p.color)?;
                self.project_changed(&p.project_id, "section.create").await;
                Ok(serde_json::to_value(section)?)
            }
            "project.section.update" => {
                let p: UpdateSectionParam = serde_json::from_value(params)?;
                let section =
                    self.projects
                        .update_section(&p.project_id, &p.section_id, p.name, p.color)?;
                self.project_changed(&p.project_id, "section.update").await;
                Ok(serde_json::to_value(section)?)
            }
            "project.section.reorder" => {
                let p: ReorderSectionParam = serde_json::from_value(params)?;
                let sections =
                    self.projects
                        .reorder_section(&p.project_id, &p.section_id, p.position)?;
                self.project_changed(&p.project_id, "section.reorder").await;
                Ok(serde_json::to_value(sections)?)
            }
            "project.section.delete" => {
                let p: SectionParam = serde_json::from_value(params)?;
                let _lifecycle = self.sessions.lifecycle.lock().await;
                anyhow::ensure!(
                    !self
                        .automation
                        .list(&Some(p.project_id.clone()))
                        .await
                        .iter()
                        .any(|job| job.job.section_id.as_deref() == Some(p.section_id.as_str())),
                    "section still owns automation jobs"
                );
                let project = self.projects.delete_section(&p.project_id, &p.section_id)?;
                self.project_changed(&p.project_id, "section.delete").await;
                Ok(serde_json::to_value(project)?)
            }
            "project.session.assign" => {
                let p: AssignSessionParam = serde_json::from_value(params)?;
                let id = SessionId::parse(p.session_id).map_err(anyhow::Error::msg)?;
                let assignment = self
                    .sessions
                    .assign(&p.project_id, p.section_id.as_deref(), &id, p.worktree_id)
                    .await?;
                self.project_changed(&p.project_id, "session.assign").await;
                Ok(serde_json::to_value(assignment)?)
            }
            "project.session.unassign" => {
                let p: AssignSessionParam = serde_json::from_value(params)?;
                let _lifecycle = self.sessions.lifecycle.lock().await;
                let (owner, _) = self
                    .projects
                    .locate_session(&p.session_id)
                    .context("session has no project")?;
                anyhow::ensure!(
                    owner.id == p.project_id,
                    "session belongs to a different project"
                );
                self.projects.unassign_session(&p.session_id)?;
                self.project_changed(&p.project_id, "session.unassign")
                    .await;
                Ok(json!({"unassigned": true}))
            }
            "project.worktree.list" => {
                let p: ProjectIdParam = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.worktree_views(&p.project_id).await?,
                )?)
            }
            "project.worktree.get" => {
                let p: WorktreeParam = serde_json::from_value(params)?;
                self.worktree_views(&p.project_id)
                    .await?
                    .into_iter()
                    .find(|view| view["worktree"]["id"] == p.worktree_id)
                    .context("worktree not found")
            }
            "project.worktree.attach" => {
                let p: AttachWorktreeParam = serde_json::from_value(params)?;
                let project = self.projects.get(&p.project_id)?;
                let repository = project
                    .repository
                    .as_ref()
                    .context("project has no repository")?;
                let info = super::git::inspect_repository(&self.profile_path(p.path)).await?;
                anyhow::ensure!(
                    info.common_dir == repository.common_dir,
                    "worktree belongs to a different repository"
                );
                let name = p.name.unwrap_or_else(|| default_worktree_name(&info.root));
                let project = self.projects.add_worktree(
                    &p.project_id,
                    worktree_record(name, info.root, WorktreeSource::External),
                )?;
                self.project_changed(&p.project_id, "worktree.attach").await;
                Ok(serde_json::to_value(project)?)
            }
            "project.worktree.create" => {
                let p: CreateWorktreeParam = serde_json::from_value(params)?;
                let project = self.projects.get(&p.project_id)?;
                let repository = project
                    .repository
                    .as_ref()
                    .context("project has no repository")?;
                let name = p.name.unwrap_or_else(|| p.branch.clone());
                let path = p
                    .path
                    .unwrap_or_else(|| project.pwd.parent().unwrap_or(&project.pwd).join(&name));
                anyhow::ensure!(!path.exists(), "worktree path already exists");
                let status = super::git::create_worktree(
                    &repository.root,
                    &path,
                    &p.branch,
                    p.start_point.as_deref(),
                )
                .await?;
                let project = self.projects.add_worktree(
                    &p.project_id,
                    worktree_record(name, status.path, WorktreeSource::Managed),
                )?;
                self.project_changed(&p.project_id, "worktree.create").await;
                Ok(serde_json::to_value(project)?)
            }
            "project.worktree.update" => {
                let p: UpdateWorktreeParam = serde_json::from_value(params)?;
                let worktree =
                    self.projects
                        .update_worktree(&p.project_id, &p.worktree_id, p.name)?;
                self.project_changed(&p.project_id, "worktree.update").await;
                Ok(serde_json::to_value(worktree)?)
            }
            "project.worktree.detach" | "project.worktree.remove" => {
                let _lifecycle = self.sessions.lifecycle.lock().await;
                let p: WorktreeParam = serde_json::from_value(params)?;
                let existing = self.projects.get(&p.project_id)?;
                let worktree = existing
                    .worktrees
                    .iter()
                    .find(|w| w.id == p.worktree_id)
                    .context("worktree not found")?;
                anyhow::ensure!(
                    worktree.source != WorktreeSource::Primary,
                    "primary worktree cannot be removed"
                );
                anyhow::ensure!(
                    !existing
                        .session_assignments
                        .iter()
                        .any(|a| a.worktree_id.as_ref() == Some(&p.worktree_id)),
                    "worktree still owns sessions"
                );
                if method == "project.worktree.remove" {
                    self.ensure_worktree_unused(&worktree.path).await?;
                    let repository = existing
                        .repository
                        .as_ref()
                        .context("project has no repository")?;
                    super::git::remove_worktree(&repository.root, &worktree.path).await?;
                }
                let project = self
                    .projects
                    .remove_worktree(&p.project_id, &p.worktree_id)?;
                self.project_changed(&p.project_id, "worktree.remove").await;
                Ok(serde_json::to_value(project)?)
            }
            _ => anyhow::bail!("unknown project method: {method}"),
        }
    }

    async fn ensure_worktree_unused(&self, path: &std::path::Path) -> Result<()> {
        for archived in [false, true] {
            let mut cursor = None;
            loop {
                let mut query = dwo_agent_service::SessionListQuery::new(cursor, Some(500));
                query.archived = archived;
                let page = self.service.list(query).await?;
                anyhow::ensure!(
                    !page
                        .sessions
                        .iter()
                        .any(|session| session.cwd.starts_with(path)),
                    "worktree is still used by an active or archived session"
                );
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        Ok(())
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
            "project already has a repository"
        );
        Ok(self.projects.set_repository(
            project_id,
            RepositoryRecord {
                root: info.root.clone(),
                common_dir: info.common_dir,
                remote_url: info.remote_url,
            },
            worktree_record(name.into(), info.root, WorktreeSource::Primary),
        )?)
    }
    async fn worktree_views(&self, project_id: &str) -> Result<Vec<Value>> {
        let project = self.projects.get(project_id)?;
        let mut views = Vec::new();
        for worktree in &project.worktrees {
            let status = super::git::worktree_status(&worktree.path).await;
            let mut sessions = Vec::new();
            for assignment in project.session_assignments.iter().filter(|assignment| {
                assignment.worktree_id.as_deref() == Some(worktree.id.as_str())
            }) {
                if let Ok(id) = SessionId::parse(assignment.session_id.clone())
                    && let Ok(snapshot) = self.service.status(&id).await
                {
                    sessions.push(snapshot);
                }
            }
            views.push(json!({"worktree": worktree, "git": status.as_ref().ok(), "available": status.is_ok(), "sessions": sessions}));
        }
        Ok(views)
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
fn default_worktree_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Worktree".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{HostSessionOptions, tests::write_test_profile};

    fn git(repo: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn worktree_remove_deletes_git_tree_and_detach_only_unregisters() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let host = Host::build(&write_test_profile(root.path())).await.unwrap();
        let project = host
            .handle_method("project.create", json!({"pwd": repo}))
            .await
            .unwrap();
        let project_id = project["id"].as_str().unwrap();
        let tree = root.path().join("tree");
        let created = host
            .handle_method(
                "project.worktree.create",
                json!({"project_id": project_id, "branch": "test-branch", "path": tree}),
            )
            .await
            .unwrap();
        let worktree_id = created["worktrees"][1]["id"].as_str().unwrap();
        let section = host
            .projects
            .create_section(project_id, "Next".into(), None)
            .unwrap();
        let id = host
            .create_session(HostSessionOptions {
                project_id: Some(project_id.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        host.handle_method(
            "project.session.assign",
            json!({"project_id": project_id, "session_id": id, "worktree_id": worktree_id}),
        )
        .await
        .unwrap();
        assert_eq!(
            host.service.snapshot(&id).await.unwrap().record.info.cwd,
            std::fs::canonicalize(&tree).unwrap()
        );
        host.handle_method(
            "project.session.assign",
            json!({"project_id": project_id, "session_id": id, "section_id": section.id}),
        )
        .await
        .unwrap();
        let assignment = host.projects.locate_session(id.as_str()).unwrap().1;
        assert_eq!(assignment.section_id, section.id);
        assert_eq!(assignment.worktree_id.as_deref(), Some(worktree_id));
        assert!(
            host.handle_method(
                "project.session.assign",
                json!({"project_id": project_id, "session_id": "session-missing"})
            )
            .await
            .is_err()
        );
        let remove = json!({"project_id": project_id, "worktree_id": worktree_id});
        assert!(
            host.handle_method("project.worktree.remove", remove.clone())
                .await
                .is_err()
        );
        host.archive_session(&id).await.unwrap();
        assert!(
            host.handle_method("project.worktree.remove", remove.clone())
                .await
                .is_err()
        );
        host.delete_session(&id).await.unwrap();
        host.handle_method("project.worktree.detach", remove)
            .await
            .unwrap();
        assert!(tree.is_dir());
        let attached = host
            .handle_method(
                "project.worktree.attach",
                json!({"project_id": project_id, "path": tree}),
            )
            .await
            .unwrap();
        let worktree_id = attached["worktrees"][1]["id"].as_str().unwrap();
        host.handle_method(
            "project.worktree.remove",
            json!({"project_id": project_id, "worktree_id": worktree_id}),
        )
        .await
        .unwrap();
        assert!(!tree.exists());
        assert_eq!(host.projects.get(project_id).unwrap().worktrees.len(), 1);
        host.shutdown().await;
    }
}
