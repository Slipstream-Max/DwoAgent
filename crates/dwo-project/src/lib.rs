use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const AGENTS_FILE: &str = "AGENTS.md";

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("project not found: {0}")]
    ProjectNotFound(String),
    #[error("section not found: {0}")]
    SectionNotFound(String),
    #[error("worktree not found: {0}")]
    WorktreeNotFound(String),
    #[error("invalid project data: {0}")]
    Invalid(String),
    #[error("project storage error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid project file {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}
pub type Result<T> = std::result::Result<T, ProjectError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub pwd: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
    pub default_section_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_assignments: Vec<SessionAssignment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktrees: Vec<WorktreeRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_worktree_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryRecord>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub id: String,
    pub name: String,
    pub color: String,
    pub order: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAssignment {
    pub session_id: String,
    pub section_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryRecord {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeSource {
    Primary,
    Managed,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRecord {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub source: WorktreeSource,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct CreateProject {
    pub name: Option<String>,
    pub pwd: PathBuf,
}

#[derive(Debug)]
pub struct ProjectService {
    root: PathBuf,
    projects: RwLock<Vec<Project>>,
}

impl ProjectService {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| ProjectError::Io {
            path: root.clone(),
            source,
        })?;
        let mut projects = Vec::new();
        for entry in fs::read_dir(&root).map_err(|source| ProjectError::Io {
            path: root.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| ProjectError::Io {
                path: root.clone(),
                source,
            })?;
            let path = entry.path().join("project.json");
            if !path.is_file() {
                continue;
            }
            let bytes = fs::read(&path).map_err(|source| ProjectError::Io {
                path: path.clone(),
                source,
            })?;
            let project: Project =
                serde_json::from_slice(&bytes).map_err(|source| ProjectError::Json {
                    path: path.clone(),
                    source,
                })?;
            validate(&project)?;
            projects.push(project);
        }
        projects.sort_by_key(|project| project.created_at_ms);
        Ok(Self {
            root,
            projects: RwLock::new(projects),
        })
    }
    pub fn list(&self) -> Vec<Project> {
        self.projects.read().expect("project lock poisoned").clone()
    }
    pub fn get(&self, id: &str) -> Result<Project> {
        self.projects
            .read()
            .expect("project lock poisoned")
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or_else(|| ProjectError::ProjectNotFound(id.into()))
    }
    pub fn create(&self, input: CreateProject) -> Result<Project> {
        let pwd = canonical_directory(&input.pwd)?;
        let name = input
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| {
                pwd.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Project".into())
            });
        let mut projects = self.projects.write().expect("project lock poisoned");
        if projects.iter().any(|project| project.pwd == pwd) {
            return Err(ProjectError::Invalid(format!(
                "a project already uses pwd: {}",
                pwd.display()
            )));
        }
        let section = Section {
            id: new_id("section"),
            name: "默认".into(),
            color: "#6b7280".into(),
            order: 0,
        };
        let now = now_ms();
        let project = Project {
            id: new_id("project"),
            name,
            pwd,
            sections: vec![section.clone()],
            default_section_id: section.id,
            session_assignments: Vec::new(),
            worktrees: Vec::new(),
            default_worktree_id: None,
            repository: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.persist(&project)?;
        projects.push(project.clone());
        Ok(project)
    }
    pub fn update_project(&self, id: &str, name: String) -> Result<Project> {
        let name = nonempty("project name", name)?;
        self.mutate(id, |project| {
            project.name = name;
            Ok(())
        })
    }
    pub fn delete(&self, id: &str) -> Result<()> {
        let mut projects = self.projects.write().expect("project lock poisoned");
        let index = projects
            .iter()
            .position(|project| project.id == id)
            .ok_or_else(|| ProjectError::ProjectNotFound(id.into()))?;
        let path = self.project_dir(&projects[index].id);
        let _permit = dwo_file_guard::permit_file(&path.join("project.json"));
        if path.exists() {
            fs::remove_dir_all(&path).map_err(|source| ProjectError::Io { path, source })?;
        }
        projects.remove(index);
        Ok(())
    }
    pub fn create_section(
        &self,
        project_id: &str,
        name: String,
        color: Option<String>,
    ) -> Result<Section> {
        let section = Section {
            id: new_id("section"),
            name: nonempty("section name", name)?,
            color: color
                .filter(|color| !color.trim().is_empty())
                .unwrap_or_else(|| "#6b7280".into()),
            order: 0,
        };
        let id = section.id.clone();
        let project = self.mutate(project_id, |project| {
            let mut section = section.clone();
            section.order = project.sections.len() as u32;
            project.sections.push(section);
            Ok(())
        })?;
        project
            .sections
            .into_iter()
            .find(|item| item.id == id)
            .ok_or(ProjectError::SectionNotFound(id))
    }
    pub fn update_section(
        &self,
        project_id: &str,
        section_id: &str,
        name: String,
        color: Option<String>,
    ) -> Result<Section> {
        let name = nonempty("section name", name)?;
        let project = self.mutate(project_id, |project| {
            let section = project
                .sections
                .iter_mut()
                .find(|section| section.id == section_id)
                .ok_or_else(|| ProjectError::SectionNotFound(section_id.into()))?;
            section.name = name;
            if let Some(color) = color.filter(|color| !color.trim().is_empty()) {
                section.color = color;
            }
            Ok(())
        })?;
        project
            .sections
            .into_iter()
            .find(|section| section.id == section_id)
            .ok_or_else(|| ProjectError::SectionNotFound(section_id.into()))
    }
    pub fn reorder_section(
        &self,
        project_id: &str,
        section_id: &str,
        position: usize,
    ) -> Result<Vec<Section>> {
        let project = self.mutate(project_id, |project| {
            let index = project
                .sections
                .iter()
                .position(|section| section.id == section_id)
                .ok_or_else(|| ProjectError::SectionNotFound(section_id.into()))?;
            let section = project.sections.remove(index);
            project
                .sections
                .insert(position.min(project.sections.len()), section);
            for (order, section) in project.sections.iter_mut().enumerate() {
                section.order = order as u32;
            }
            Ok(())
        })?;
        Ok(project.sections)
    }
    pub fn delete_section(&self, project_id: &str, section_id: &str) -> Result<Project> {
        self.mutate(project_id, |project| {
            if project.default_section_id == section_id {
                return Err(ProjectError::Invalid(
                    "default section cannot be deleted".into(),
                ));
            }
            let before = project.sections.len();
            project.sections.retain(|section| section.id != section_id);
            if before == project.sections.len() {
                return Err(ProjectError::SectionNotFound(section_id.into()));
            }
            if project
                .session_assignments
                .iter()
                .any(|assignment| assignment.section_id == section_id)
            {
                return Err(ProjectError::Invalid("section still owns sessions".into()));
            }
            Ok(())
        })
    }
    pub fn assign_session(
        &self,
        project_id: &str,
        section_id: Option<&str>,
        session_id: String,
        worktree_id: Option<String>,
    ) -> Result<SessionAssignment> {
        let session_id = nonempty("session id", session_id)?;
        let mut projects = self.projects.write().expect("project lock poisoned");
        if projects.iter().any(|p| {
            p.id != project_id
                && p.session_assignments
                    .iter()
                    .any(|a| a.session_id == session_id)
        }) {
            return Err(ProjectError::Invalid(
                "session already belongs to a different project".into(),
            ));
        }
        let index = projects
            .iter()
            .position(|p| p.id == project_id)
            .ok_or_else(|| ProjectError::ProjectNotFound(project_id.into()))?;
        let mut project = projects[index].clone();
        let existing = project
            .session_assignments
            .iter()
            .find(|a| a.session_id == session_id);
        let assignment = SessionAssignment {
            section_id: section_id
                .map(str::to_owned)
                .or_else(|| existing.map(|a| a.section_id.clone()))
                .unwrap_or_else(|| project.default_section_id.clone()),
            worktree_id: worktree_id.or_else(|| existing.and_then(|a| a.worktree_id.clone())),
            session_id,
        };
        project
            .session_assignments
            .retain(|a| a.session_id != assignment.session_id);
        project.session_assignments.push(assignment.clone());
        project.updated_at_ms = now_ms();
        validate(&project)?;
        self.persist(&project)?;
        projects[index] = project;
        Ok(assignment)
    }
    pub fn unassign_session(&self, session_id: &str) -> Result<()> {
        let mut projects = self.projects.write().expect("project lock poisoned");
        let Some(project) = projects.iter_mut().find(|p| {
            p.session_assignments
                .iter()
                .any(|a| a.session_id == session_id)
        }) else {
            return Ok(());
        };
        let mut updated = project.clone();
        updated
            .session_assignments
            .retain(|a| a.session_id != session_id);
        updated.updated_at_ms = now_ms();
        self.persist(&updated)?;
        *project = updated;
        Ok(())
    }
    pub fn locate_session(&self, session_id: &str) -> Option<(Project, SessionAssignment)> {
        self.projects
            .read()
            .expect("project lock poisoned")
            .iter()
            .find_map(|project| {
                project
                    .session_assignments
                    .iter()
                    .find(|a| a.session_id == session_id)
                    .map(|a| (project.clone(), a.clone()))
            })
    }
    pub fn project_rule_path(&self, project_id: &str) -> Result<PathBuf> {
        Ok(self.get(project_id)?.pwd.join(AGENTS_FILE))
    }
    pub fn set_project_rule(&self, project_id: &str, content: &str) -> Result<()> {
        atomic_write(&self.project_rule_path(project_id)?, content.as_bytes())
    }
    pub fn project_rule(&self, project_id: &str) -> Result<String> {
        let path = self.project_rule_path(project_id)?;
        match fs::read_to_string(&path) {
            Ok(content) => Ok(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(source) => Err(ProjectError::Io { path, source }),
        }
    }
    pub fn set_repository(
        &self,
        project_id: &str,
        repository: RepositoryRecord,
        primary: WorktreeRecord,
    ) -> Result<Project> {
        self.mutate(project_id, |project| {
            project.repository = Some(repository);
            project.default_worktree_id = (project.pwd == primary.path).then(|| primary.id.clone());
            project.worktrees = vec![primary];
            Ok(())
        })
    }
    pub fn add_worktree(&self, project_id: &str, worktree: WorktreeRecord) -> Result<Project> {
        self.mutate(project_id, |project| {
            if project
                .worktrees
                .iter()
                .any(|item| item.id == worktree.id || item.path == worktree.path)
            {
                return Err(ProjectError::Invalid(
                    "worktree is already registered".into(),
                ));
            }
            project.worktrees.push(worktree);
            Ok(())
        })
    }
    pub fn update_worktree(
        &self,
        project_id: &str,
        worktree_id: &str,
        name: String,
    ) -> Result<WorktreeRecord> {
        let project = self.mutate(project_id, |project| {
            let worktree = project
                .worktrees
                .iter_mut()
                .find(|item| item.id == worktree_id)
                .ok_or_else(|| ProjectError::WorktreeNotFound(worktree_id.into()))?;
            worktree.name = nonempty("worktree name", name)?;
            Ok(())
        })?;
        project
            .worktrees
            .into_iter()
            .find(|item| item.id == worktree_id)
            .ok_or_else(|| ProjectError::WorktreeNotFound(worktree_id.into()))
    }
    pub fn remove_worktree(&self, project_id: &str, worktree_id: &str) -> Result<Project> {
        self.mutate(project_id, |project| {
            if project
                .worktrees
                .iter()
                .any(|w| w.id == worktree_id && w.source == WorktreeSource::Primary)
            {
                return Err(ProjectError::Invalid(
                    "default worktree cannot be removed".into(),
                ));
            }
            let before = project.worktrees.len();
            project.worktrees.retain(|item| item.id != worktree_id);
            if before == project.worktrees.len() {
                return Err(ProjectError::WorktreeNotFound(worktree_id.into()));
            }
            if project
                .session_assignments
                .iter()
                .any(|a| a.worktree_id.as_deref() == Some(worktree_id))
            {
                return Err(ProjectError::Invalid("worktree still owns sessions".into()));
            }
            Ok(())
        })
    }
    fn mutate(
        &self,
        project_id: &str,
        apply: impl FnOnce(&mut Project) -> Result<()>,
    ) -> Result<Project> {
        let mut projects = self.projects.write().expect("project lock poisoned");
        let index = projects
            .iter()
            .position(|project| project.id == project_id)
            .ok_or_else(|| ProjectError::ProjectNotFound(project_id.into()))?;
        let mut project = projects[index].clone();
        apply(&mut project)?;
        project.updated_at_ms = now_ms();
        validate(&project)?;
        self.persist(&project)?;
        projects[index] = project.clone();
        Ok(project)
    }
    fn project_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
    fn persist(&self, project: &Project) -> Result<()> {
        let dir = self.project_dir(&project.id);
        fs::create_dir_all(&dir).map_err(|source| ProjectError::Io {
            path: dir.clone(),
            source,
        })?;
        let path = dir.join("project.json");
        let bytes = serde_json::to_vec_pretty(project).map_err(|source| ProjectError::Json {
            path: path.clone(),
            source,
        })?;
        let permit = dwo_file_guard::permit_file(&path);
        let result = atomic_write(&path, &bytes);
        drop(permit);
        result
    }
}

fn validate(project: &Project) -> Result<()> {
    if project.id.trim().is_empty() || project.name.trim().is_empty() || !project.pwd.is_absolute()
    {
        return Err(ProjectError::Invalid(
            "project id, name and absolute pwd are required".into(),
        ));
    }
    let section_ids = project
        .sections
        .iter()
        .map(|section| section.id.as_str())
        .collect::<HashSet<_>>();
    if section_ids.len() != project.sections.len()
        || !section_ids.contains(project.default_section_id.as_str())
        || project
            .sections
            .iter()
            .any(|section| section.name.trim().is_empty() || section.color.trim().is_empty())
    {
        return Err(ProjectError::Invalid("invalid project sections".into()));
    }
    let worktree_ids = project
        .worktrees
        .iter()
        .map(|worktree| worktree.id.as_str())
        .collect::<HashSet<_>>();
    if project
        .default_worktree_id
        .as_deref()
        .is_some_and(|id| !worktree_ids.contains(id))
        || project.session_assignments.iter().any(|assignment| {
            !section_ids.contains(assignment.section_id.as_str())
                || assignment
                    .worktree_id
                    .as_deref()
                    .is_some_and(|id| !worktree_ids.contains(id))
        })
    {
        return Err(ProjectError::Invalid("invalid project assignments".into()));
    }
    let sessions = project
        .session_assignments
        .iter()
        .map(|assignment| assignment.session_id.as_str())
        .collect::<HashSet<_>>();
    if sessions.len() != project.session_assignments.len() {
        return Err(ProjectError::Invalid(
            "a session can have only one project assignment".into(),
        ));
    }
    Ok(())
}
fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !path.is_dir() {
        return Err(ProjectError::Invalid(format!(
            "project pwd is not a directory: {}",
            path.display()
        )));
    }
    Ok(path)
}
fn nonempty(field: &str, value: String) -> Result<String> {
    if value.trim().is_empty() {
        Err(ProjectError::Invalid(format!("{field} cannot be empty")))
    } else {
        Ok(value)
    }
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temp, bytes).map_err(|source| ProjectError::Io {
        path: temp.clone(),
        source,
    })?;
    fs::rename(&temp, path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignments_move_sections_and_preserve_valid_state_on_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = ProjectService::open(root.path().join("storage")).unwrap();
        let project = store
            .create(CreateProject {
                name: None,
                pwd: root.path().into(),
            })
            .unwrap();
        assert_eq!(project.sections.len(), 1);
        let first = store
            .assign_session(&project.id, None, "session-test".into(), None)
            .unwrap();
        assert_eq!(first.section_id, project.default_section_id);
        let section = store
            .create_section(&project.id, "Next".into(), Some("red".into()))
            .unwrap();
        store
            .assign_session(&project.id, Some(&section.id), "session-test".into(), None)
            .unwrap();
        assert!(
            store
                .assign_session(&project.id, Some("missing"), "session-test".into(), None)
                .is_err()
        );
        let reopened = ProjectService::open(root.path().join("storage")).unwrap();
        assert_eq!(
            reopened
                .locate_session("session-test")
                .unwrap()
                .1
                .section_id,
            section.id
        );
        assert!(store.delete_section(&project.id, &section.id).is_err());
        assert!(
            store
                .delete_section(&project.id, &project.default_section_id)
                .is_err()
        );
    }

    #[test]
    fn delete_releases_protection_and_keeps_project_directory() {
        let root = tempfile::tempdir().unwrap();
        let _protection = dwo_file_guard::Protection::install(root.path(), &["runtime"], &[]);
        let store = ProjectService::open(root.path().join("runtime/projects")).unwrap();
        let project = store
            .create(CreateProject {
                name: None,
                pwd: root.path().into(),
            })
            .unwrap();
        store.set_project_rule(&project.id, "rules").unwrap();
        store.delete(&project.id).unwrap();
        assert!(store.list().is_empty());
        assert!(
            !root
                .path()
                .join("runtime/projects")
                .join(project.id)
                .exists()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("AGENTS.md")).unwrap(),
            "rules"
        );
    }
}
