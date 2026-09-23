use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use dwo_agent_service::{
    NewSession, SessionId, SessionLlmSettings, SessionService, SessionWorkspace,
};
use dwo_project::{Project, ProjectService};
use dwo_tools::SessionMode;
use tokio::sync::Mutex;

use super::{HostSessionOptions, managed_workspace_path};

/// Host-owned coordination shared by interactive sessions and automation triggers.
pub(crate) struct SessionManager {
    pub(crate) lifecycle: Mutex<()>,
    service: Arc<SessionService>,
    projects: Arc<ProjectService>,
    root: PathBuf,
    defaults: RwLock<(SessionLlmSettings, SessionMode)>,
}

impl SessionManager {
    pub(crate) fn new(
        service: Arc<SessionService>,
        projects: Arc<ProjectService>,
        root: PathBuf,
        llm: SessionLlmSettings,
        mode: SessionMode,
    ) -> Self {
        Self {
            lifecycle: Mutex::new(()),
            service,
            projects,
            root,
            defaults: RwLock::new((llm, mode)),
        }
    }

    pub(crate) fn defaults(&self) -> (SessionLlmSettings, SessionMode) {
        self.defaults
            .read()
            .expect("session defaults lock poisoned")
            .clone()
    }

    pub(crate) fn apply_defaults(&self, llm: SessionLlmSettings, mode: SessionMode) {
        *self
            .defaults
            .write()
            .expect("session defaults lock poisoned") = (llm, mode);
    }

    pub(crate) async fn assign(
        &self,
        project_id: &str,
        section_id: Option<&str>,
        id: &SessionId,
        worktree_id: Option<String>,
    ) -> Result<dwo_project::SessionAssignment> {
        let _lifecycle = self.lifecycle.lock().await;
        self.service.ensure_active(id).await?;
        let snapshot = self.service.snapshot(id).await?;
        let previous = self.projects.locate_session(id.as_str());
        anyhow::ensure!(
            previous.as_ref().is_none_or(|(p, _)| p.id == project_id),
            "unassign the session before assigning a different project"
        );
        let project = self.projects.get(project_id)?;
        let worktree_id = worktree_id.or_else(|| previous.and_then(|(_, a)| a.worktree_id));
        let (pwd, selected_worktree) = project_directory(&project, section_id, worktree_id)?;
        let changed = snapshot.record.info.cwd != pwd;
        if changed {
            self.service
                .set_workspace(id, SessionWorkspace::External { pwd: pwd.clone() }, pwd)
                .await?;
        }
        match self.projects.assign_session(
            project_id,
            section_id,
            id.to_string(),
            selected_worktree,
        ) {
            Ok(assignment) => Ok(assignment),
            Err(error) => {
                if changed {
                    self.service
                        .set_workspace(id, snapshot.record.info.workspace, snapshot.record.info.cwd)
                        .await
                        .context("restore session workspace after assignment failed")?;
                }
                Err(error.into())
            }
        }
    }

    pub(crate) async fn create(&self, options: HostSessionOptions) -> Result<SessionId> {
        let _lifecycle = self.lifecycle.lock().await;
        anyhow::ensure!(
            options.project_id.is_none() || options.cwd.is_none(),
            "cwd cannot be supplied with project_id"
        );
        anyhow::ensure!(
            options.section_id.is_none() || options.project_id.is_some(),
            "section_id requires project_id"
        );
        anyhow::ensure!(
            options.worktree_id.is_none() || options.project_id.is_some(),
            "worktree_id requires project_id"
        );
        anyhow::ensure!(
            options.from.is_none() || (options.project_id.is_none() && options.cwd.is_none()),
            "forks inherit their source workspace"
        );
        let id = SessionId::new();
        let is_fork = options.from.is_some();
        let source_id = options.from.as_ref().or(options.parent_session_id.as_ref());
        let source = match source_id {
            Some(id) => Some(self.service.snapshot(id).await?),
            None => None,
        };
        let mut assignment = None;
        let workspace = if let Some(project_id) = &options.project_id {
            let project = self.projects.get(project_id)?;
            let section_id = options
                .section_id
                .clone()
                .unwrap_or_else(|| project.default_section_id.clone());
            let (pwd, worktree_id) =
                project_directory(&project, Some(&section_id), options.worktree_id)?;
            assignment = Some((project.id, section_id, worktree_id));
            SessionWorkspace::External { pwd }
        } else if let Some(source) = source.as_ref().filter(|_| options.cwd.is_none()) {
            assignment = self
                .projects
                .locate_session(source.record.info.id.as_str())
                .map(|(project, assignment)| {
                    (project.id, assignment.section_id, assignment.worktree_id)
                });
            if is_fork {
                source.record.info.workspace.clone()
            } else {
                // A child uses its parent's working directory; it does not own that directory.
                SessionWorkspace::External {
                    pwd: source.record.info.cwd.clone(),
                }
            }
        } else if let Some(cwd) = &options.cwd {
            let pwd = if cwd.is_absolute() {
                cwd.clone()
            } else {
                self.root.join(cwd)
            };
            SessionWorkspace::External {
                pwd: std::fs::canonicalize(pwd)?,
            }
        } else {
            SessionWorkspace::Managed
        };
        let cwd = match &workspace {
            SessionWorkspace::Managed => managed_workspace_path(&self.root, &id),
            SessionWorkspace::External { pwd } => pwd.clone(),
        };
        let (inherited_llm, inherited_mode) = source
            .as_ref()
            .map(|s| (s.record.llm.clone(), s.record.info.mode))
            .unwrap_or_else(|| self.defaults());
        let mode = options.mode.unwrap_or(inherited_mode);
        if let Some(parent_id) = &options.parent_session_id {
            let parent_mode = if source_id == Some(parent_id) {
                source
                    .as_ref()
                    .expect("parent snapshot loaded")
                    .record
                    .info
                    .mode
            } else {
                self.service.snapshot(parent_id).await?.record.info.mode
            };
            ensure_policy_ceiling(mode, parent_mode)?
        }
        let llm = match options.model {
            Some(model) => SessionLlmSettings::new(model, options.reasoning),
            None => SessionLlmSettings::new(
                inherited_llm.model,
                options.reasoning.or(inherited_llm.reasoning),
            ),
        };
        let result = async {
            if workspace == SessionWorkspace::Managed {
                if let Some(source) = &source {
                    copy_workspace(&source.record.info.cwd, &cwd)?;
                } else {
                    std::fs::create_dir_all(&cwd)?;
                }
            }
            self.service
                .create(NewSession {
                    from: options.from,
                    id: Some(id.clone()),
                    parent_session_id: options.parent_session_id,
                    title: options.title,
                    workspace: Some(workspace.clone()),
                    cwd: Some(cwd.clone()),
                    mode: Some(mode),
                    llm: Some(llm),
                    ephemeral: options.ephemeral,
                })
                .await?;
            if let Some((project_id, section_id, worktree_id)) = assignment {
                self.projects.assign_session(
                    &project_id,
                    Some(&section_id),
                    id.to_string(),
                    worktree_id,
                )?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = self.service.delete(&id).await;
            if workspace == SessionWorkspace::Managed && cwd.is_dir() {
                let _ = std::fs::remove_dir_all(&cwd);
            }
            return Err(error);
        }
        Ok(id)
    }
}

pub(super) fn ensure_policy_ceiling(requested: SessionMode, parent: SessionMode) -> Result<()> {
    let rank = |mode| match mode {
        SessionMode::Watch => 0,
        SessionMode::Confirm => 1,
        SessionMode::FullAccess => 2,
    };
    anyhow::ensure!(
        rank(requested) <= rank(parent),
        "subsession policy {requested:?} exceeds parent policy {parent:?}"
    );
    Ok(())
}

fn project_directory(
    project: &Project,
    section_id: Option<&str>,
    worktree_id: Option<String>,
) -> Result<(PathBuf, Option<String>)> {
    if let Some(section_id) = section_id {
        anyhow::ensure!(
            project.sections.iter().any(|s| s.id == section_id),
            "section not found in project: {section_id}"
        );
    }
    let worktree_id = worktree_id.or_else(|| project.default_worktree_id.clone());
    let pwd = match &worktree_id {
        Some(id) => project
            .worktrees
            .iter()
            .find(|w| &w.id == id)
            .context("worktree not found")?
            .path
            .clone(),
        None => project.pwd.clone(),
    };
    Ok((pwd, worktree_id))
}

fn copy_workspace(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_workspace(&source_path, &target_path)?;
        } else {
            std::fs::copy(&source_path, &target_path)?;
        }
    }
    Ok(())
}
