use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const OVERVIEW_FILE: &str = "overview.md";
pub const AGENTS_FILE: &str = "AGENTS.md";
pub const AUTOMATION_DIR: &str = "automation";
pub const AUTOMATION_CONFIG_FILE: &str = "config.yaml";
pub const AUTOMATION_HISTORY_FILE: &str = "history.yaml";
pub const UNASSIGNED_PROJECT_ID: &str = "project-unassigned";
pub const UNASSIGNED_PROJECT_NAME: &str = "未分配会话";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    Shared,
    Independent,
}

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("project not found: {0}")]
    ProjectNotFound(String),
    #[error("section not found: {0}")]
    SectionNotFound(String),
    #[error("topic not found: {0}")]
    TopicNotFound(String),
    #[error("label not found: {0}")]
    LabelNotFound(String),
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
    pub kind: ProjectKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktrees: Vec<WorktreeRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_worktree_id: Option<String>,
    pub board: Board,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Board {
    pub uncategorized_section_id: String,
    pub uncategorized_topic_id: String,
    pub sections: Vec<Section>,
    pub topics: Vec<Topic>,
    pub labels: Vec<Label>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub id: String,
    pub name: String,
    pub order: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Topic {
    pub id: String,
    pub section_id: String,
    pub title: String,
    pub order: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub label_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Label {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreateProject {
    pub name: String,
    pub kind: ProjectKind,
    pub pwd: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ProjectService {
    root: PathBuf,
    projects: RwLock<Vec<Project>>,
}

impl ProjectService {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        create_dir_all(&root)?;
        let mut projects = Vec::new();
        for entry in read_dir(&root)? {
            let entry = entry.map_err(|source| ProjectError::Io {
                path: root.clone(),
                source,
            })?;
            if !entry.path().is_dir() {
                continue;
            }
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
            validate_project(&project)?;
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

    pub fn get(&self, project_id: &str) -> Result<Project> {
        self.projects
            .read()
            .expect("project lock poisoned")
            .iter()
            .find(|project| project.id == project_id)
            .cloned()
            .ok_or_else(|| ProjectError::ProjectNotFound(project_id.to_string()))
    }

    pub fn create(&self, input: CreateProject) -> Result<Project> {
        let name = nonempty("project name", input.name)?;
        let pwd = input.pwd.as_deref().map(canonical_directory).transpose()?;
        validate_project_location(input.kind, pwd.as_deref())?;
        let mut projects = self.projects.write().expect("project lock poisoned");
        if let Some(pwd) = &pwd
            && projects
                .iter()
                .any(|project| project.pwd.as_deref() == Some(pwd.as_path()))
        {
            return Err(ProjectError::Invalid(format!(
                "a project already uses pwd: {}",
                pwd.display()
            )));
        }
        self.create_locked(&mut projects, name, input.kind, pwd)
    }

    pub fn get_or_create_by_pwd(&self, name: String, pwd: &Path) -> Result<Project> {
        let name = nonempty("project name", name)?;
        let pwd = canonical_directory(pwd)?;
        let mut projects = self.projects.write().expect("project lock poisoned");
        if let Some(project) = projects
            .iter()
            .find(|project| project.pwd.as_deref() == Some(pwd.as_path()))
        {
            return Ok(project.clone());
        }
        self.create_locked(&mut projects, name, ProjectKind::Shared, Some(pwd))
    }

    pub fn get_or_create_unassigned(&self) -> Result<Project> {
        if let Some(project) = self
            .projects
            .read()
            .expect("project lock poisoned")
            .iter()
            .find(|project| project.id == UNASSIGNED_PROJECT_ID)
            .cloned()
        {
            return Ok(project);
        }
        let mut projects = self.projects.write().expect("project lock poisoned");
        if let Some(project) = projects
            .iter()
            .find(|project| project.id == UNASSIGNED_PROJECT_ID)
            .cloned()
        {
            return Ok(project);
        }
        self.create_locked_with_id(
            &mut projects,
            UNASSIGNED_PROJECT_ID.to_string(),
            UNASSIGNED_PROJECT_NAME.to_string(),
            ProjectKind::Independent,
            None,
        )
    }

    fn create_locked(
        &self,
        projects: &mut Vec<Project>,
        name: String,
        kind: ProjectKind,
        pwd: Option<PathBuf>,
    ) -> Result<Project> {
        self.create_locked_with_id(&mut *projects, new_id("project"), name, kind, pwd)
    }

    fn create_locked_with_id(
        &self,
        projects: &mut Vec<Project>,
        id: String,
        name: String,
        kind: ProjectKind,
        pwd: Option<PathBuf>,
    ) -> Result<Project> {
        let project_dir = self.project_dir(&id);
        create_dir_all(&project_dir)?;
        let section_id = new_id("section");
        let topic_id = new_id("topic");
        let now = unix_time_ms();
        let project = Project {
            id,
            name,
            kind,
            pwd,
            repository: None,
            worktrees: Vec::new(),
            default_worktree_id: None,
            board: Board {
                uncategorized_section_id: section_id.clone(),
                uncategorized_topic_id: topic_id.clone(),
                sections: vec![Section {
                    id: section_id.clone(),
                    name: "未分类".to_string(),
                    order: 0,
                }],
                topics: vec![Topic {
                    id: topic_id.clone(),
                    section_id,
                    title: "未分类".to_string(),
                    order: 0,
                    session_ids: Vec::new(),
                    label_ids: Vec::new(),
                }],
                labels: Vec::new(),
            },
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.create_topic_files(&project.id, &topic_id)?;
        self.persist(&project)?;
        projects.push(project.clone());
        Ok(project)
    }

    pub fn update_project(&self, project_id: &str, name: String) -> Result<Project> {
        let name = nonempty("project name", name)?;
        self.mutate(project_id, |project| {
            project.name = name;
            Ok(())
        })
    }

    pub fn set_repository(
        &self,
        project_id: &str,
        repository: RepositoryRecord,
        primary: WorktreeRecord,
    ) -> Result<Project> {
        self.mutate(project_id, |project| {
            if project.kind != ProjectKind::Shared {
                return Err(ProjectError::Invalid(
                    "independent projects cannot attach repositories".to_string(),
                ));
            }
            project.pwd = Some(primary.path.clone());
            project.repository = Some(repository);
            project.default_worktree_id = Some(primary.id.clone());
            project.worktrees = vec![primary];
            Ok(())
        })
    }

    pub fn add_worktree(&self, project_id: &str, worktree: WorktreeRecord) -> Result<Project> {
        self.mutate(project_id, |project| {
            if project.kind != ProjectKind::Shared {
                return Err(ProjectError::Invalid(
                    "independent projects cannot register worktrees".to_string(),
                ));
            }
            if project.repository.is_none() {
                return Err(ProjectError::Invalid(
                    "project has no attached repository".to_string(),
                ));
            }
            if project
                .worktrees
                .iter()
                .any(|existing| existing.id == worktree.id || existing.path == worktree.path)
            {
                return Err(ProjectError::Invalid(format!(
                    "worktree is already registered: {}",
                    worktree.path.display()
                )));
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
        let name = nonempty("worktree name", name)?;
        let worktree_id = worktree_id.to_string();
        let project = self.mutate(project_id, |project| {
            let worktree = project
                .worktrees
                .iter_mut()
                .find(|worktree| worktree.id == worktree_id)
                .ok_or_else(|| ProjectError::WorktreeNotFound(worktree_id.clone()))?;
            worktree.name = name;
            Ok(())
        })?;
        project
            .worktrees
            .into_iter()
            .find(|worktree| worktree.id == worktree_id)
            .ok_or(ProjectError::WorktreeNotFound(worktree_id))
    }

    pub fn remove_worktree(&self, project_id: &str, worktree_id: &str) -> Result<Project> {
        let worktree_id = worktree_id.to_string();
        self.mutate(project_id, |project| {
            let before = project.worktrees.len();
            project
                .worktrees
                .retain(|worktree| worktree.id != worktree_id);
            if project.worktrees.len() == before {
                return Err(ProjectError::WorktreeNotFound(worktree_id.clone()));
            }
            if project.default_worktree_id.as_deref() == Some(worktree_id.as_str()) {
                project.default_worktree_id = project
                    .worktrees
                    .first()
                    .map(|worktree| worktree.id.clone());
            }
            Ok(())
        })
    }

    pub fn create_section(&self, project_id: &str, name: String) -> Result<Section> {
        let name = nonempty("section name", name)?;
        let section = Section {
            id: new_id("section"),
            name,
            order: 0,
        };
        let result = section.clone();
        self.mutate(project_id, |project| {
            let mut section = section;
            section.order = project.board.sections.len() as u32;
            project.board.sections.push(section);
            Ok(())
        })?;
        Ok(result_with_section_order(self.get(project_id)?, &result.id))
    }

    pub fn update_section(
        &self,
        project_id: &str,
        section_id: &str,
        name: String,
    ) -> Result<Section> {
        let name = nonempty("section name", name)?;
        let section_id = section_id.to_string();
        let project = self.mutate(project_id, |project| {
            let section = project
                .board
                .sections
                .iter_mut()
                .find(|section| section.id == section_id)
                .ok_or_else(|| ProjectError::SectionNotFound(section_id.clone()))?;
            section.name = name;
            Ok(())
        })?;
        find_section(&project, &section_id).cloned()
    }

    pub fn reorder_section(
        &self,
        project_id: &str,
        section_id: &str,
        position: usize,
    ) -> Result<Vec<Section>> {
        let section_id = section_id.to_string();
        let project = self.mutate(project_id, |project| {
            move_item(&mut project.board.sections, &section_id, position, |item| {
                &item.id
            })
            .ok_or_else(|| ProjectError::SectionNotFound(section_id.clone()))?;
            normalize_orders(&mut project.board.sections, |item, order| {
                item.order = order
            });
            Ok(())
        })?;
        Ok(project.board.sections)
    }

    pub fn delete_section(&self, project_id: &str, section_id: &str) -> Result<Project> {
        let section_id = section_id.to_string();
        self.mutate(project_id, |project| {
            if section_id == project.board.uncategorized_section_id {
                return Err(ProjectError::Invalid(
                    "the uncategorized section cannot be deleted".to_string(),
                ));
            }
            let before = project.board.sections.len();
            project
                .board
                .sections
                .retain(|section| section.id != section_id);
            if before == project.board.sections.len() {
                return Err(ProjectError::SectionNotFound(section_id.clone()));
            }
            let uncategorized = project.board.uncategorized_section_id.clone();
            let next_order = project
                .board
                .topics
                .iter()
                .filter(|topic| topic.section_id == uncategorized)
                .count() as u32;
            for (next_order, topic) in (next_order..).zip(
                project
                    .board
                    .topics
                    .iter_mut()
                    .filter(|topic| topic.section_id == section_id),
            ) {
                topic.section_id = uncategorized.clone();
                topic.order = next_order;
            }
            normalize_orders(&mut project.board.sections, |item, order| {
                item.order = order
            });
            Ok(())
        })
    }

    pub fn create_topic(&self, project_id: &str, section_id: &str, title: String) -> Result<Topic> {
        let title = nonempty("topic title", title)?;
        let topic_id = new_id("topic");
        let section_id = section_id.to_string();
        let project = self.mutate(project_id, |project| {
            ensure_section(project, &section_id)?;
            let order = project
                .board
                .topics
                .iter()
                .filter(|topic| topic.section_id == section_id)
                .count() as u32;
            project.board.topics.push(Topic {
                id: topic_id.clone(),
                section_id: section_id.clone(),
                title,
                order,
                session_ids: Vec::new(),
                label_ids: Vec::new(),
            });
            Ok(())
        })?;
        self.create_topic_files(project_id, &topic_id)?;
        find_topic(&project, &topic_id).cloned()
    }

    pub fn update_topic(&self, project_id: &str, topic_id: &str, title: String) -> Result<Topic> {
        let title = nonempty("topic title", title)?;
        let topic_id = topic_id.to_string();
        let project = self.mutate(project_id, |project| {
            find_topic_mut(project, &topic_id)?.title = title;
            Ok(())
        })?;
        find_topic(&project, &topic_id).cloned()
    }

    pub fn move_topic(
        &self,
        project_id: &str,
        topic_id: &str,
        section_id: &str,
        position: usize,
    ) -> Result<Topic> {
        let topic_id = topic_id.to_string();
        let section_id = section_id.to_string();
        let project = self.mutate(project_id, |project| {
            ensure_section(project, &section_id)?;
            let old_section = find_topic(project, &topic_id)?.section_id.clone();
            let index = project
                .board
                .topics
                .iter()
                .position(|topic| topic.id == topic_id)
                .expect("topic checked above");
            let mut topic = project.board.topics.remove(index);
            topic.section_id = section_id.clone();
            let mut target_indices = project
                .board
                .topics
                .iter()
                .enumerate()
                .filter_map(|(index, topic)| (topic.section_id == section_id).then_some(index))
                .collect::<Vec<_>>();
            let insert = if position >= target_indices.len() {
                target_indices
                    .last()
                    .map_or(project.board.topics.len(), |index| index + 1)
            } else {
                target_indices.remove(position)
            };
            project.board.topics.insert(insert, topic);
            normalize_topic_orders(&mut project.board.topics, &old_section);
            normalize_topic_orders(&mut project.board.topics, &section_id);
            Ok(())
        })?;
        find_topic(&project, &topic_id).cloned()
    }

    pub fn move_topic_to_project(
        &self,
        source_project_id: &str,
        topic_id: &str,
        target_project_id: &str,
        target_section_id: &str,
        position: usize,
    ) -> Result<Topic> {
        if source_project_id == target_project_id {
            return self.move_topic(source_project_id, topic_id, target_section_id, position);
        }

        let topic_id = topic_id.to_string();
        let target_section_id = target_section_id.to_string();
        let mut projects = self.projects.write().expect("project lock poisoned");
        let source_index = projects
            .iter()
            .position(|project| project.id == source_project_id)
            .ok_or_else(|| ProjectError::ProjectNotFound(source_project_id.to_string()))?;
        let target_index = projects
            .iter()
            .position(|project| project.id == target_project_id)
            .ok_or_else(|| ProjectError::ProjectNotFound(target_project_id.to_string()))?;

        let source_topic = projects[source_index]
            .board
            .topics
            .iter()
            .find(|topic| topic.id == topic_id)
            .cloned()
            .ok_or_else(|| ProjectError::TopicNotFound(topic_id.clone()))?;
        if source_topic.id == projects[source_index].board.uncategorized_topic_id {
            return Err(ProjectError::Invalid(
                "the uncategorized topic cannot move across projects".to_string(),
            ));
        }
        if projects[target_index]
            .board
            .topics
            .iter()
            .any(|topic| topic.id == topic_id)
        {
            return Err(ProjectError::Invalid(format!(
                "topic is already registered in target project: {topic_id}"
            )));
        }
        if !projects[target_index]
            .board
            .sections
            .iter()
            .any(|section| section.id == target_section_id)
        {
            return Err(ProjectError::SectionNotFound(target_section_id));
        }
        let (source, target) = if source_index < target_index {
            let (left, right) = projects.split_at_mut(target_index);
            (&mut left[source_index], &mut right[0])
        } else {
            let (left, right) = projects.split_at_mut(source_index);
            (&mut right[0], &mut left[target_index])
        };

        let source_topic_index = source
            .board
            .topics
            .iter()
            .position(|topic| topic.id == topic_id)
            .expect("source topic was checked above");
        let mut topic = source.board.topics.remove(source_topic_index);
        let old_section_id = topic.section_id.clone();
        let label_ids = topic
            .label_ids
            .iter()
            .filter_map(|label_id| {
                source
                    .board
                    .labels
                    .iter()
                    .find(|label| &label.id == label_id)
                    .cloned()
            })
            .map(|label| {
                target
                    .board
                    .labels
                    .iter()
                    .find(|existing| {
                        existing.name == label.name
                            && existing.color == label.color
                            && existing.description == label.description
                    })
                    .map(|existing| existing.id.clone())
                    .unwrap_or_else(|| {
                        let id = new_id("label");
                        target.board.labels.push(Label {
                            id: id.clone(),
                            name: label.name,
                            color: label.color,
                            description: label.description,
                        });
                        id
                    })
            })
            .collect::<Vec<_>>();
        topic.section_id = target_section_id.clone();
        topic.label_ids = label_ids;
        topic.order = target
            .board
            .topics
            .iter()
            .filter(|candidate| candidate.section_id == target_section_id)
            .count() as u32;
        let target_indices = target
            .board
            .topics
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                (candidate.section_id == target_section_id).then_some(index)
            })
            .collect::<Vec<_>>();
        let insert = if position >= target_indices.len() {
            target_indices
                .last()
                .map_or(target.board.topics.len(), |index| index + 1)
        } else {
            target_indices[position]
        };
        target.board.topics.insert(insert, topic.clone());
        normalize_topic_orders(&mut source.board.topics, &old_section_id);
        normalize_topic_orders(&mut target.board.topics, &target_section_id);
        source.updated_at_ms = unix_time_ms();
        target.updated_at_ms = unix_time_ms();
        validate_project(source)?;
        validate_project(target)?;

        let source_topic_dir = self.topic_dir(source_project_id, &topic_id);
        let target_topic_dir = self.topic_dir(target_project_id, &topic_id);
        if source_topic_dir.is_dir() {
            create_dir_all(
                target_topic_dir
                    .parent()
                    .expect("topic directory has a project parent"),
            )?;
            fs::rename(&source_topic_dir, &target_topic_dir).map_err(|source| {
                ProjectError::Io {
                    path: target_topic_dir.clone(),
                    source,
                }
            })?;
        } else {
            self.create_topic_files(target_project_id, &topic_id)?;
        }
        self.persist(source)?;
        self.persist(target)?;
        Ok(topic)
    }

    pub fn delete_topic(&self, project_id: &str, topic_id: &str) -> Result<Project> {
        let topic_id = topic_id.to_string();
        let project = self.mutate(project_id, |project| {
            if topic_id == project.board.uncategorized_topic_id {
                return Err(ProjectError::Invalid(
                    "the uncategorized topic cannot be deleted".to_string(),
                ));
            }
            let index = project
                .board
                .topics
                .iter()
                .position(|topic| topic.id == topic_id)
                .ok_or_else(|| ProjectError::TopicNotFound(topic_id.clone()))?;
            let removed = project.board.topics.remove(index);
            let uncategorized_id = project.board.uncategorized_topic_id.clone();
            let uncategorized = find_topic_mut(project, &uncategorized_id)?;
            append_unique(&mut uncategorized.session_ids, removed.session_ids);
            normalize_topic_orders(&mut project.board.topics, &removed.section_id);
            Ok(())
        })?;
        let topic_dir = self.topic_dir(project_id, &topic_id);
        if topic_dir.is_dir() {
            fs::remove_dir_all(&topic_dir).map_err(|source| ProjectError::Io {
                path: topic_dir,
                source,
            })?;
        }
        Ok(project)
    }

    pub fn assign_session(&self, project_id: &str, topic_id: &str, id: String) -> Result<Topic> {
        let id = nonempty("session id", id)?;
        self.unassign_session_everywhere(&id)?;
        let topic_id = topic_id.to_string();
        let project = self.mutate(project_id, |project| {
            let topic = find_topic_mut(project, &topic_id)?;
            push_unique(&mut topic.session_ids, id);
            Ok(())
        })?;
        find_topic(&project, &topic_id).cloned()
    }

    pub fn unassign_session(&self, project_id: &str, topic_id: &str, id: &str) -> Result<Topic> {
        let topic_id = topic_id.to_string();
        let id = id.to_string();
        let project = self.mutate(project_id, |project| {
            let topic = find_topic_mut(project, &topic_id)?;
            topic.session_ids.retain(|value| value != &id);
            Ok(())
        })?;
        find_topic(&project, &topic_id).cloned()
    }

    pub fn create_label(
        &self,
        project_id: &str,
        name: String,
        color: String,
        description: Option<String>,
    ) -> Result<Label> {
        let label = Label {
            id: new_id("label"),
            name: nonempty("label name", name)?,
            color: nonempty("label color", color)?,
            description: clean_optional(description),
        };
        let result = label.clone();
        self.mutate(project_id, |project| {
            project.board.labels.push(label);
            Ok(())
        })?;
        Ok(result)
    }

    pub fn update_label(
        &self,
        project_id: &str,
        label_id: &str,
        name: String,
        color: String,
        description: Option<String>,
    ) -> Result<Label> {
        let label_id = label_id.to_string();
        let name = nonempty("label name", name)?;
        let color = nonempty("label color", color)?;
        let project = self.mutate(project_id, |project| {
            let label = project
                .board
                .labels
                .iter_mut()
                .find(|label| label.id == label_id)
                .ok_or_else(|| ProjectError::LabelNotFound(label_id.clone()))?;
            label.name = name;
            label.color = color;
            label.description = clean_optional(description);
            Ok(())
        })?;
        project
            .board
            .labels
            .into_iter()
            .find(|label| label.id == label_id)
            .ok_or(ProjectError::LabelNotFound(label_id))
    }

    pub fn delete_label(&self, project_id: &str, label_id: &str) -> Result<Project> {
        let label_id = label_id.to_string();
        self.mutate(project_id, |project| {
            let before = project.board.labels.len();
            project.board.labels.retain(|label| label.id != label_id);
            if before == project.board.labels.len() {
                return Err(ProjectError::LabelNotFound(label_id.clone()));
            }
            for topic in &mut project.board.topics {
                topic.label_ids.retain(|id| id != &label_id);
            }
            Ok(())
        })
    }

    pub fn assign_label(&self, project_id: &str, topic_id: &str, label_id: &str) -> Result<Topic> {
        let topic_id = topic_id.to_string();
        let label_id = label_id.to_string();
        let project = self.mutate(project_id, |project| {
            if !project
                .board
                .labels
                .iter()
                .any(|label| label.id == label_id)
            {
                return Err(ProjectError::LabelNotFound(label_id.clone()));
            }
            let topic = find_topic_mut(project, &topic_id)?;
            push_unique(&mut topic.label_ids, label_id);
            Ok(())
        })?;
        find_topic(&project, &topic_id).cloned()
    }

    pub fn unassign_label(
        &self,
        project_id: &str,
        topic_id: &str,
        label_id: &str,
    ) -> Result<Topic> {
        let topic_id = topic_id.to_string();
        let label_id = label_id.to_string();
        let project = self.mutate(project_id, |project| {
            find_topic_mut(project, &topic_id)?
                .label_ids
                .retain(|id| id != &label_id);
            Ok(())
        })?;
        find_topic(&project, &topic_id).cloned()
    }

    pub fn overview(&self, project_id: &str, topic_id: &str) -> Result<String> {
        self.read_topic_file(project_id, topic_id, OVERVIEW_FILE)
    }

    pub fn set_overview(&self, project_id: &str, topic_id: &str, content: &str) -> Result<()> {
        self.write_topic_file(project_id, topic_id, OVERVIEW_FILE, content)
    }

    pub fn agents(&self, project_id: &str, topic_id: &str) -> Result<String> {
        self.read_topic_file(project_id, topic_id, AGENTS_FILE)
    }

    pub fn set_agents(&self, project_id: &str, topic_id: &str, content: &str) -> Result<()> {
        self.write_topic_file(project_id, topic_id, AGENTS_FILE, content)
    }

    pub fn agents_path(&self, project_id: &str, topic_id: &str) -> Result<PathBuf> {
        self.ensure_topic(project_id, topic_id)?;
        Ok(self.topic_dir(project_id, topic_id).join(AGENTS_FILE))
    }

    fn project_automation_dir(&self, project_id: &str) -> Result<PathBuf> {
        self.ensure_project(project_id)?;
        Ok(self.project_dir(project_id).join(AUTOMATION_DIR))
    }

    pub fn automation_config_path(&self, project_id: &str) -> Result<PathBuf> {
        Ok(self
            .project_automation_dir(project_id)?
            .join(AUTOMATION_CONFIG_FILE))
    }

    pub fn automation_history_path(&self, project_id: &str) -> Result<PathBuf> {
        Ok(self
            .project_automation_dir(project_id)?
            .join(AUTOMATION_HISTORY_FILE))
    }

    pub fn locate_session(&self, session_id: &str) -> Option<(Project, Topic)> {
        self.projects
            .read()
            .expect("project lock poisoned")
            .iter()
            .find_map(|project| {
                project
                    .board
                    .topics
                    .iter()
                    .find(|topic| topic.session_ids.iter().any(|id| id == session_id))
                    .cloned()
                    .map(|topic| (project.clone(), topic))
            })
    }

    pub fn unassign_session_everywhere(&self, session_id: &str) -> Result<()> {
        let project_ids = self
            .projects
            .read()
            .expect("project lock poisoned")
            .iter()
            .filter(|project| {
                project.board.topics.iter().any(|topic| {
                    topic
                        .session_ids
                        .iter()
                        .any(|existing| existing == session_id)
                })
            })
            .map(|project| project.id.clone())
            .collect::<Vec<_>>();
        for project_id in project_ids {
            self.mutate(&project_id, |project| {
                for topic in &mut project.board.topics {
                    topic.session_ids.retain(|existing| existing != session_id);
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn mutate(
        &self,
        project_id: &str,
        apply: impl FnOnce(&mut Project) -> Result<()>,
    ) -> Result<Project> {
        let mut projects = self.projects.write().expect("project lock poisoned");
        let project = projects
            .iter_mut()
            .find(|project| project.id == project_id)
            .ok_or_else(|| ProjectError::ProjectNotFound(project_id.to_string()))?;
        apply(project)?;
        project.updated_at_ms = unix_time_ms();
        validate_project(project)?;
        self.persist(project)?;
        Ok(project.clone())
    }

    fn ensure_project(&self, project_id: &str) -> Result<()> {
        self.get(project_id).map(|_| ())
    }

    fn ensure_topic(&self, project_id: &str, topic_id: &str) -> Result<()> {
        let project = self.get(project_id)?;
        find_topic(&project, topic_id).map(|_| ())
    }

    fn read_topic_file(&self, project_id: &str, topic_id: &str, name: &str) -> Result<String> {
        self.ensure_topic(project_id, topic_id)?;
        let path = self.topic_dir(project_id, topic_id).join(name);
        fs::read_to_string(&path).map_err(|source| ProjectError::Io { path, source })
    }

    fn write_topic_file(
        &self,
        project_id: &str,
        topic_id: &str,
        name: &str,
        content: &str,
    ) -> Result<()> {
        self.ensure_topic(project_id, topic_id)?;
        let path = self.topic_dir(project_id, topic_id).join(name);
        atomic_write(&path, content.as_bytes())
    }

    fn persist(&self, project: &Project) -> Result<()> {
        let path = self.project_dir(&project.id).join("project.json");
        let bytes = serde_json::to_vec_pretty(project).map_err(|source| ProjectError::Json {
            path: path.clone(),
            source,
        })?;
        atomic_write(&path, &bytes)
    }

    fn create_topic_files(&self, project_id: &str, topic_id: &str) -> Result<()> {
        let directory = self.topic_dir(project_id, topic_id);
        create_dir_all(&directory)?;
        atomic_write(&directory.join(OVERVIEW_FILE), b"")?;
        atomic_write(&directory.join(AGENTS_FILE), b"")
    }

    fn project_dir(&self, project_id: &str) -> PathBuf {
        self.root.join(project_id)
    }

    fn topic_dir(&self, project_id: &str, topic_id: &str) -> PathBuf {
        self.project_dir(project_id).join("topics").join(topic_id)
    }
}

fn validate_project(project: &Project) -> Result<()> {
    if project.id.trim().is_empty() || project.name.trim().is_empty() {
        return Err(ProjectError::Invalid(
            "project id and name are required".to_string(),
        ));
    }
    validate_project_location(project.kind, project.pwd.as_deref())?;
    if project.kind == ProjectKind::Independent
        && (project.repository.is_some()
            || !project.worktrees.is_empty()
            || project.default_worktree_id.is_some())
    {
        return Err(ProjectError::Invalid(
            "independent projects cannot own repositories or worktrees".to_string(),
        ));
    }
    let worktree_ids = project
        .worktrees
        .iter()
        .map(|worktree| worktree.id.as_str())
        .collect::<HashSet<_>>();
    let worktree_paths = project
        .worktrees
        .iter()
        .map(|worktree| &worktree.path)
        .collect::<HashSet<_>>();
    if worktree_ids.len() != project.worktrees.len()
        || worktree_paths.len() != project.worktrees.len()
        || project
            .default_worktree_id
            .as_deref()
            .is_some_and(|id| !worktree_ids.contains(id))
        || (project.repository.is_none() && !project.worktrees.is_empty())
        || project.worktrees.iter().any(|worktree| {
            worktree.id.trim().is_empty()
                || worktree.name.trim().is_empty()
                || !worktree.path.is_absolute()
        })
    {
        return Err(ProjectError::Invalid(
            "invalid project worktree records".to_string(),
        ));
    }
    let section_ids = project
        .board
        .sections
        .iter()
        .map(|section| section.id.as_str())
        .collect::<HashSet<_>>();
    if section_ids.len() != project.board.sections.len()
        || !section_ids.contains(project.board.uncategorized_section_id.as_str())
    {
        return Err(ProjectError::Invalid("invalid board sections".to_string()));
    }
    let topic_ids = project
        .board
        .topics
        .iter()
        .map(|topic| topic.id.as_str())
        .collect::<HashSet<_>>();
    if topic_ids.len() != project.board.topics.len()
        || !topic_ids.contains(project.board.uncategorized_topic_id.as_str())
        || project
            .board
            .topics
            .iter()
            .any(|topic| !section_ids.contains(topic.section_id.as_str()))
    {
        return Err(ProjectError::Invalid("invalid board topics".to_string()));
    }
    let label_ids = project
        .board
        .labels
        .iter()
        .map(|label| label.id.as_str())
        .collect::<HashSet<_>>();
    if label_ids.len() != project.board.labels.len()
        || project
            .board
            .topics
            .iter()
            .flat_map(|topic| &topic.label_ids)
            .any(|id| !label_ids.contains(id.as_str()))
    {
        return Err(ProjectError::Invalid("invalid board labels".to_string()));
    }
    let mut sessions = HashSet::new();
    if project
        .board
        .topics
        .iter()
        .any(|topic| topic.session_ids.iter().any(|id| !sessions.insert(id)))
    {
        return Err(ProjectError::Invalid(
            "a session can belong to only one topic in a project".to_string(),
        ));
    }
    Ok(())
}

fn validate_project_location(kind: ProjectKind, pwd: Option<&Path>) -> Result<()> {
    match (kind, pwd) {
        (ProjectKind::Shared, Some(pwd)) if pwd.is_absolute() => Ok(()),
        (ProjectKind::Shared, _) => Err(ProjectError::Invalid(
            "shared projects require an absolute pwd".to_string(),
        )),
        (ProjectKind::Independent, None) => Ok(()),
        (ProjectKind::Independent, Some(_)) => Err(ProjectError::Invalid(
            "independent projects cannot define pwd".to_string(),
        )),
    }
}

fn ensure_section(project: &Project, section_id: &str) -> Result<()> {
    find_section(project, section_id).map(|_| ())
}

fn find_section<'a>(project: &'a Project, id: &str) -> Result<&'a Section> {
    project
        .board
        .sections
        .iter()
        .find(|section| section.id == id)
        .ok_or_else(|| ProjectError::SectionNotFound(id.to_string()))
}

fn find_topic<'a>(project: &'a Project, id: &str) -> Result<&'a Topic> {
    project
        .board
        .topics
        .iter()
        .find(|topic| topic.id == id)
        .ok_or_else(|| ProjectError::TopicNotFound(id.to_string()))
}

fn find_topic_mut<'a>(project: &'a mut Project, id: &str) -> Result<&'a mut Topic> {
    project
        .board
        .topics
        .iter_mut()
        .find(|topic| topic.id == id)
        .ok_or_else(|| ProjectError::TopicNotFound(id.to_string()))
}

fn result_with_section_order(project: Project, id: &str) -> Section {
    project
        .board
        .sections
        .into_iter()
        .find(|section| section.id == id)
        .expect("new section is present")
}

fn move_item<T>(
    items: &mut Vec<T>,
    id: &str,
    position: usize,
    key: impl Fn(&T) -> &String,
) -> Option<()> {
    let index = items.iter().position(|item| key(item) == id)?;
    let item = items.remove(index);
    let position = position.min(items.len());
    items.insert(position, item);
    Some(())
}

fn normalize_orders<T>(items: &mut [T], set: impl Fn(&mut T, u32)) {
    for (index, item) in items.iter_mut().enumerate() {
        set(item, index as u32);
    }
}

fn normalize_topic_orders(topics: &mut [Topic], section_id: &str) {
    for (order, topic) in topics
        .iter_mut()
        .filter(|topic| topic.section_id == section_id)
        .enumerate()
    {
        topic.order = order as u32;
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn append_unique(values: &mut Vec<String>, incoming: Vec<String>) {
    for value in incoming {
        push_unique(values, value);
    }
}

fn nonempty(field: &str, value: String) -> Result<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(ProjectError::Invalid(format!("{field} is required")))
    } else {
        Ok(value)
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return Err(ProjectError::Invalid(format!(
            "project pwd is not a directory: {}",
            path.display()
        )));
    }
    fs::canonicalize(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn create_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_dir(path: &Path) -> Result<fs::ReadDir> {
    fs::read_dir(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).map_err(|source| ProjectError::Io {
        path: temporary.clone(),
        source,
    })?;
    if path.exists() {
        fs::remove_file(path).map_err(|source| ProjectError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    fs::rename(&temporary, path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_default_board_without_embedding_a_workspace() {
        let root = tempfile::tempdir().unwrap();
        let service = ProjectService::open(root.path().join("projects")).unwrap();
        let project = service
            .create(CreateProject {
                name: "Demo".to_string(),
                kind: ProjectKind::Independent,
                pwd: None,
            })
            .unwrap();

        assert_eq!(project.pwd, None);
        assert!(
            !root
                .path()
                .join("projects")
                .join(&project.id)
                .join("workspace")
                .exists()
        );
        assert_eq!(project.board.sections.len(), 1);
        assert_eq!(project.board.topics.len(), 1);
        let topic = &project.board.topics[0];
        assert_eq!(topic.id, project.board.uncategorized_topic_id);
        assert_eq!(service.agents(&project.id, &topic.id).unwrap(), "");
        assert_eq!(service.overview(&project.id, &topic.id).unwrap(), "");
        assert!(!root.path().join("workspaces").exists());

        let stored: serde_json::Value = serde_json::from_slice(
            &fs::read(
                root.path()
                    .join("projects")
                    .join(&project.id)
                    .join("project.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(stored["kind"], "independent");
        assert!(stored.get("pwd").is_none());
        assert!(stored.get("workspaces").is_none());
    }

    #[test]
    fn rejects_invalid_project_kind_and_location_combinations() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let service = ProjectService::open(root.path().join("projects")).unwrap();

        assert!(matches!(
            service.create(CreateProject {
                name: "Missing pwd".to_string(),
                kind: ProjectKind::Shared,
                pwd: None,
            }),
            Err(ProjectError::Invalid(_))
        ));
        assert!(matches!(
            service.create(CreateProject {
                name: "Unexpected pwd".to_string(),
                kind: ProjectKind::Independent,
                pwd: Some(workspace),
            }),
            Err(ProjectError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_unknown_project_fields() {
        let root = tempfile::tempdir().unwrap();
        let projects = root.path().join("projects");
        let project_dir = projects.join("project-invalid");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("project.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": "project-invalid",
                "name": "Invalid",
                "kind": "independent",
                "workspaces": [],
                "board": {
                    "uncategorizedSectionId": "section-inbox",
                    "uncategorizedTopicId": "topic-inbox",
                    "sections": [{"id": "section-inbox", "name": "Inbox", "order": 0}],
                    "topics": [{
                        "id": "topic-inbox",
                        "sectionId": "section-inbox",
                        "title": "Inbox",
                        "order": 0
                    }],
                    "labels": []
                },
                "createdAtMs": 1,
                "updatedAtMs": 1
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(matches!(
            ProjectService::open(projects),
            Err(ProjectError::Json { .. })
        ));
    }

    #[test]
    fn persists_board_markdown_and_relations() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("existing");
        fs::create_dir_all(&workspace).unwrap();
        let service = ProjectService::open(root.path().join("projects")).unwrap();
        let project = service
            .create(CreateProject {
                name: "Demo".to_string(),
                kind: ProjectKind::Shared,
                pwd: Some(workspace.clone()),
            })
            .unwrap();
        let section = service
            .create_section(&project.id, "Build".to_string())
            .unwrap();
        let topic = service
            .create_topic(&project.id, &section.id, "Project API".to_string())
            .unwrap();
        let label = service
            .create_label(
                &project.id,
                "Backend".to_string(),
                "#388E3C".to_string(),
                None,
            )
            .unwrap();
        service
            .assign_label(&project.id, &topic.id, &label.id)
            .unwrap();
        service
            .assign_session(&project.id, &topic.id, "session-1".to_string())
            .unwrap();
        service
            .set_overview(&project.id, &topic.id, "# Plan")
            .unwrap();
        service
            .set_agents(&project.id, &topic.id, "Stay scoped.")
            .unwrap();
        let common_dir = root.path().join("repo.git");
        fs::create_dir_all(&common_dir).unwrap();
        service
            .set_repository(
                &project.id,
                RepositoryRecord {
                    root: fs::canonicalize(&workspace).unwrap(),
                    common_dir: fs::canonicalize(common_dir).unwrap(),
                    remote_url: Some("https://example.test/repo.git".to_string()),
                },
                WorktreeRecord {
                    id: "worktree-local".to_string(),
                    name: "Local".to_string(),
                    path: fs::canonicalize(&workspace).unwrap(),
                    source: WorktreeSource::Primary,
                    created_at_ms: 1,
                },
            )
            .unwrap();

        let reloaded = ProjectService::open(root.path().join("projects")).unwrap();
        let loaded = reloaded.get(&project.id).unwrap();
        let loaded_topic = find_topic(&loaded, &topic.id).unwrap();
        assert_eq!(loaded_topic.session_ids, ["session-1"]);
        assert_eq!(loaded_topic.label_ids, [label.id]);
        assert_eq!(reloaded.overview(&project.id, &topic.id).unwrap(), "# Plan");
        assert_eq!(
            reloaded.agents(&project.id, &topic.id).unwrap(),
            "Stay scoped."
        );
        assert_eq!(loaded.pwd, Some(fs::canonicalize(workspace).unwrap()));
        assert_eq!(
            loaded.default_worktree_id.as_deref(),
            Some("worktree-local")
        );
        assert_eq!(loaded.worktrees.len(), 1);
        assert_eq!(
            loaded.repository.unwrap().remote_url.as_deref(),
            Some("https://example.test/repo.git")
        );
    }

    #[test]
    fn moves_topic_between_projects_and_preserves_resources() {
        let root = tempfile::tempdir().unwrap();
        let service = ProjectService::open(root.path().join("projects")).unwrap();
        let source = service
            .create(CreateProject {
                name: "Source".to_string(),
                kind: ProjectKind::Independent,
                pwd: None,
            })
            .unwrap();
        let target = service
            .create(CreateProject {
                name: "Target".to_string(),
                kind: ProjectKind::Independent,
                pwd: None,
            })
            .unwrap();
        let source_section = service
            .create_section(&source.id, "Doing".to_string())
            .unwrap();
        let target_section = service
            .create_section(&target.id, "Inbox".to_string())
            .unwrap();
        let topic = service
            .create_topic(&source.id, &source_section.id, "Move me".to_string())
            .unwrap();
        let label = service
            .create_label(
                &source.id,
                "Backend".to_string(),
                "#388E3C".to_string(),
                Some("source label".to_string()),
            )
            .unwrap();
        service
            .assign_label(&source.id, &topic.id, &label.id)
            .unwrap();
        service
            .assign_session(&source.id, &topic.id, "session-1".to_string())
            .unwrap();
        service
            .set_overview(&source.id, &topic.id, "# Plan")
            .unwrap();
        service
            .set_agents(&source.id, &topic.id, "Use backend rules")
            .unwrap();

        let moved = service
            .move_topic_to_project(
                &source.id,
                &topic.id,
                &target.id,
                &target_section.id,
                usize::MAX,
            )
            .unwrap();

        assert_eq!(moved.id, topic.id);
        assert_eq!(moved.section_id, target_section.id);
        assert_eq!(moved.session_ids, ["session-1"]);
        assert!(
            service
                .get(&source.id)
                .unwrap()
                .board
                .topics
                .iter()
                .all(|candidate| candidate.id != topic.id)
        );
        assert_eq!(service.overview(&target.id, &topic.id).unwrap(), "# Plan");
        assert_eq!(
            service.agents(&target.id, &topic.id).unwrap(),
            "Use backend rules"
        );
        let target_project = service.get(&target.id).unwrap();
        let target_topic = target_project
            .board
            .topics
            .iter()
            .find(|candidate| candidate.id == topic.id)
            .unwrap();
        assert_eq!(target_topic.label_ids.len(), 1);
        assert_eq!(target_project.board.labels.len(), 1);
    }

    #[test]
    fn moving_reference_keeps_single_topic_owner() {
        let root = tempfile::tempdir().unwrap();
        let service = ProjectService::open(root.path()).unwrap();
        let project = service
            .create(CreateProject {
                name: "Demo".to_string(),
                kind: ProjectKind::Independent,
                pwd: None,
            })
            .unwrap();
        let section = service
            .create_section(&project.id, "Work".to_string())
            .unwrap();
        let first = service
            .create_topic(&project.id, &section.id, "One".to_string())
            .unwrap();
        let second = service
            .create_topic(&project.id, &section.id, "Two".to_string())
            .unwrap();
        service
            .assign_session(&project.id, &first.id, "session-1".to_string())
            .unwrap();
        service
            .assign_session(&project.id, &second.id, "session-1".to_string())
            .unwrap();

        let project = service.get(&project.id).unwrap();
        assert!(
            find_topic(&project, &first.id)
                .unwrap()
                .session_ids
                .is_empty()
        );
        assert_eq!(
            find_topic(&project, &second.id).unwrap().session_ids,
            ["session-1"]
        );
    }

    #[test]
    fn get_or_create_by_pwd_is_atomic_and_explicit_duplicates_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let service =
            std::sync::Arc::new(ProjectService::open(root.path().join("projects")).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|index| {
                let service = service.clone();
                let barrier = barrier.clone();
                let workspace = workspace.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    service
                        .get_or_create_by_pwd(format!("Project {index}"), &workspace)
                        .unwrap()
                        .id
                })
            })
            .collect::<Vec<_>>();
        let ids = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), 1);
        assert_eq!(service.list().len(), 1);

        let duplicate = service.create(CreateProject {
            name: "Duplicate".to_string(),
            kind: ProjectKind::Shared,
            pwd: Some(workspace),
        });
        assert!(matches!(duplicate, Err(ProjectError::Invalid(_))));
    }
}
