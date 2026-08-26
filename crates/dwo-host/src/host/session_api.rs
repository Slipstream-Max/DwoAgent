use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use dwo_agent_service::{
    EndpointId, NotificationLevel, PromptAccepted, SessionConfigUpdate, SessionEventPayload,
    SessionId, SessionListQuery, SessionLlmSettings, SessionNotification, SessionService,
    SessionSubscription,
};
use dwo_context::MessageContent;
use dwo_project::CreateProject;
use dwo_tools::{ConfirmationDecision, SessionMode};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Host, HostSessionOptions};
use dwo_command::{
    AvailableSkill, DirectiveKinds, directive_kinds, expand as expand_prompt_directives,
};

#[derive(Deserialize)]
struct SessionIdParam {
    session_id: String,
}

#[derive(Deserialize)]
struct NewSessionParam {
    title: Option<String>,
    cwd: Option<PathBuf>,
    project_id: Option<String>,
    topic_id: Option<String>,
}

#[derive(Deserialize)]
struct ListSessionParam {
    #[serde(default)]
    all: bool,
    caller_session_id: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
pub(super) struct PromptParam {
    pub(super) session_id: Option<String>,
    pub(super) from_session_id: Option<String>,
    pub(super) caller_session_id: Option<String>,
    pub(super) endpoint_id: String,
    pub(super) message: PromptMessage,
    pub(super) title: Option<String>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) policy: Option<SessionMode>,
    pub(super) model: Option<String>,
    pub(super) reasoning: Option<String>,
    #[serde(default)]
    pub(super) ephemeral: bool,
}

#[derive(Deserialize)]
struct SessionCommandParam {
    session_id: String,
    endpoint_id: String,
}

#[derive(Deserialize)]
struct NotificationParam {
    session_id: String,
    endpoint_id: Option<String>,
    category: String,
    level: NotificationLevel,
    text: String,
    #[serde(default)]
    data: Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum PromptMessage {
    Text(String),
    Content(MessageContent),
}

impl PromptMessage {
    pub(super) fn into_content(self) -> MessageContent {
        match self {
            Self::Text(text) => MessageContent::text(text),
            Self::Content(content) => content,
        }
    }
}

#[derive(Deserialize)]
struct ReadSessionParam {
    session_id: String,
    cursor: Option<usize>,
    #[serde(default = "default_read_limit")]
    limit: usize,
}

fn default_read_limit() -> usize {
    3
}

#[derive(Deserialize)]
struct CancelParam {
    session_id: String,
    turn_id: Option<String>,
}

#[derive(Deserialize)]
struct SessionConfigOptionParam {
    session_id: String,
    config_id: String,
    value: Value,
}

#[derive(Deserialize)]
struct PermissionParam {
    session_id: String,
    endpoint_id: String,
    request_id: String,
    allowed: bool,
    reason: Option<String>,
}

impl Host {
    pub async fn create_session(&self, options: HostSessionOptions) -> Result<SessionId> {
        anyhow::ensure!(
            options.project_id.is_none() || options.cwd.is_none(),
            "cwd cannot be supplied with project_id"
        );
        anyhow::ensure!(
            options.topic_id.is_none() || options.project_id.is_some(),
            "topic_id requires project_id"
        );
        let is_fork = options.from.is_some();
        let binding = if let Some(project_id) = &options.project_id {
            let project = self.projects.get(project_id)?;
            let topic_id = options
                .topic_id
                .clone()
                .unwrap_or_else(|| project.board.uncategorized_topic_id.clone());
            anyhow::ensure!(
                project
                    .board
                    .topics
                    .iter()
                    .any(|topic| topic.id == topic_id),
                "topic not found in project: {topic_id}"
            );
            Some((project, topic_id))
        } else if let Some(source_id) = &options.from {
            self.projects
                .locate_session(source_id.as_str())
                .map(|(project, topic)| (project, topic.id))
        } else {
            let pwd = options.cwd.clone().map(|cwd| {
                if cwd.is_absolute() {
                    cwd
                } else {
                    self.profile_root.join(cwd)
                }
            });
            let project_name = options
                .title
                .clone()
                .or_else(|| {
                    pwd.as_ref()
                        .and_then(|pwd| pwd.file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "Untitled Project".to_string());
            let project = match pwd {
                Some(pwd) => self.projects.get_or_create_by_pwd(project_name, &pwd)?,
                None => self.projects.create(CreateProject {
                    name: project_name,
                    pwd: None,
                })?,
            };
            let topic_id = project.board.uncategorized_topic_id.clone();
            Some((project, topic_id))
        };
        let (default_model, default_reasoning, default_mode) = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .defaults();
        let id = SessionId::new();
        let (cwd, external_rule_files) = match &binding {
            Some((project, topic_id)) => (
                (!is_fork).then(|| project.pwd.clone()),
                vec![dwo_context::ExternalRuleFile::new(
                    self.projects.agents_path(&project.id, topic_id)?,
                    project.pwd.clone(),
                )],
            ),
            _ => (None, Vec::new()),
        };
        self.service
            .create(dwo_agent_service::NewSession {
                from: options.from,
                id: Some(id.clone()),
                parent_session_id: options.parent_session_id,
                title: options.title,
                cwd,
                external_rule_files,
                mode: options.mode.or((!is_fork).then_some(default_mode)),
                llm: options.llm.or_else(|| {
                    (!is_fork).then(|| SessionLlmSettings::new(default_model, default_reasoning))
                }),
                ephemeral: options.ephemeral,
            })
            .await?;
        if let Some((project, topic_id)) = binding
            && let Err(error) = self
                .projects
                .assign_session(&project.id, &topic_id, id.to_string())
        {
            let _ = self.service.delete(&id).await;
            return Err(error.into());
        }
        Ok(id)
    }

    pub async fn delete_session(&self, id: &SessionId) -> Result<()> {
        self.service.delete(id).await?;
        cleanup_deleted_session_resources(&self.profile_root, id).await?;
        self.projects.unassign_session_everywhere(id.as_str())?;
        Ok(())
    }

    pub(crate) async fn session_count(&self) -> Result<usize> {
        let mut count = 0;
        let mut cursor = None;
        loop {
            let page = self
                .service
                .list(SessionListQuery::new(cursor, Some(500)))
                .await?;
            count += page.sessions.len();
            let Some(next) = page.next_cursor else {
                return Ok(count);
            };
            cursor = Some(next);
        }
    }

    pub(crate) async fn dispatch_session(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        match method {
            "session.list" => {
                let params: ListSessionParam = serde_json::from_value(params)?;
                let cursor = params
                    .cursor
                    .as_deref()
                    .map(str::parse::<usize>)
                    .transpose()
                    .context("session list cursor must be a non-negative integer")?;
                let mut query = SessionListQuery::new(cursor, params.limit);
                query.cwd = params.cwd;
                if !params.all {
                    query.parent_session_id =
                        parse_optional_session(params.caller_session_id.clone())?;
                    query.roots_only = query.parent_session_id.is_none();
                }
                Ok(serde_json::to_value(self.service.list(query).await?)?)
            }
            "session.status" => {
                let id = parse_session(params)?;
                Ok(serde_json::to_value(self.service.status(&id).await?)?)
            }
            "session.snapshot" => {
                let id = parse_session(params)?;
                Ok(serde_json::to_value(self.service.snapshot(&id).await?)?)
            }
            "session.prompt-directives" => {
                let id = parse_session(params)?;
                self.prompt_directive_options(&id).await
            }
            "session.notify" => {
                let params: NotificationParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let origin = params
                    .endpoint_id
                    .map(EndpointId::parse)
                    .transpose()
                    .map_err(anyhow::Error::msg)?;
                let message_id = self
                    .service
                    .publish_notification(
                        &id,
                        SessionNotification {
                            origin,
                            category: params.category,
                            level: params.level,
                            text: params.text,
                            data: params.data,
                        },
                    )
                    .await?;
                Ok(json!({"session_id": id, "message_id": message_id}))
            }
            "session.new" => {
                let params: NewSessionParam = serde_json::from_value(params)?;
                let id = self
                    .create_session(HostSessionOptions {
                        title: params.title,
                        cwd: params.cwd,
                        project_id: params.project_id,
                        topic_id: params.topic_id,
                        ..HostSessionOptions::default()
                    })
                    .await?;
                let snapshot = self.service.snapshot(&id).await?;
                Ok(json!({
                    "session_id": id,
                    "usage": snapshot.usage,
                }))
            }
            "session.fork" => {
                let source_id = parse_session(params)?;
                let id = self
                    .create_session(HostSessionOptions {
                        from: Some(source_id.clone()),
                        ..HostSessionOptions::default()
                    })
                    .await?;
                let usage = self.service.snapshot(&id).await?.usage;
                let message_id = self
                    .service
                    .publish_notification(
                        &source_id,
                        SessionNotification {
                            origin: None,
                            category: "fork_completed".to_string(),
                            level: dwo_agent_service::NotificationLevel::Success,
                            text: format!("Forked session {id}."),
                            data: json!({"forkedSessionId": id}),
                        },
                    )
                    .await?;
                Ok(json!({
                    "accepted": false,
                    "session_id": id.clone(),
                    "forked_session_id": id,
                    "message_id": message_id,
                    "usage": usage,
                }))
            }
            "session.delete" => {
                let id = parse_session(params)?;
                self.delete_session(&id).await?;
                Ok(json!({"deleted": true}))
            }
            "session.keep" => {
                let id = parse_session(params)?;
                let changed = self.service.keep(&id).await?;
                Ok(json!({"session_id": id, "persistent": true, "changed": changed}))
            }
            "session.close" => {
                let id = parse_session(params)?;
                self.service.unload(&id).await?;
                Ok(json!({"closed": true}))
            }
            "session.prompt" => {
                let params: PromptParam = serde_json::from_value(params)?;
                let caller = parse_optional_session(params.caller_session_id.clone())?;
                let (session_id, parent_id) = self.resolve_prompt_session(&params, caller).await?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                let subscription = self.service.subscribe(&session_id, None).await?;
                let content = self
                    .expand_prompt_directives(
                        &subscription.snapshot.record.info.cwd,
                        params.message.into_content(),
                    )
                    .await?;
                let accepted = self.service.prompt(&session_id, endpoint, content).await?;
                if let Some(parent_id) = parent_id {
                    spawn_result_delivery(
                        self.service.clone(),
                        subscription,
                        session_id.clone(),
                        parent_id,
                        accepted.turn_id.clone(),
                    );
                }
                Ok(json!({
                    "session_id": session_id,
                    "message_id": accepted.message_id,
                    "turn_id": accepted.turn_id,
                }))
            }
            "session.compact" => {
                let params: SessionCommandParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                let accepted = self.service.compact(&id, endpoint).await?;
                Ok(json!({
                    "session_id": id,
                    "compaction_id": accepted.compaction_id,
                }))
            }
            "session.resume-turn" => {
                let params: SessionCommandParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                let turn_id = self
                    .service
                    .prompt_internal(
                        &id,
                        MessageContent::text(
                            "<resume>Continue the previous task from the current session state.</resume>",
                        ),
                    )
                    .await?;
                Ok(json!({
                    "accepted": true,
                    "session_id": id,
                    "turn_id": turn_id,
                }))
            }
            "session.read" => {
                let params: ReadSessionParam = serde_json::from_value(params)?;
                anyhow::ensure!(
                    params.limit > 0 && params.limit <= 100,
                    "limit must be between 1 and 100"
                );
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let snapshot = self.service.snapshot(&id).await?;
                let total = snapshot.transcript.len();
                let content = snapshot
                    .transcript
                    .into_iter()
                    .enumerate()
                    .filter(|(_, event)| is_content_event(&event.payload))
                    .collect::<Vec<_>>();
                let messages = if let Some(cursor) = params.cursor {
                    content
                        .into_iter()
                        .filter(|(index, _)| *index >= cursor.min(total))
                        .take(params.limit)
                        .collect::<Vec<_>>()
                } else {
                    content
                        .into_iter()
                        .rev()
                        .take(params.limit)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                };
                let start = messages
                    .first()
                    .map_or(params.cursor.unwrap_or(total), |(index, _)| *index);
                let next_cursor = messages.last().map_or(start, |(index, _)| index + 1);
                Ok(json!({
                    "session_id": id,
                    "cursor": start,
                    "next_cursor": next_cursor,
                    "messages": messages.into_iter().map(|(cursor, event)| json!({"cursor": cursor, "event": event})).collect::<Vec<_>>(),
                }))
            }
            "session.cancel" => {
                let params: CancelParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let turn = params
                    .turn_id
                    .map(dwo_context::TurnId::parse)
                    .transpose()
                    .map_err(anyhow::Error::msg)?;
                self.service.cancel(&id, turn).await?;
                Ok(json!({"cancelled": true}))
            }
            "session.set_config_option" => {
                let params: SessionConfigOptionParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let update = match params.config_id.as_str() {
                    "model" => SessionConfigUpdate::Model(
                        params
                            .value
                            .as_str()
                            .context("model config value must be a string")?
                            .to_string(),
                    ),
                    "reasoning_mode" => SessionConfigUpdate::Reasoning(Some(
                        params
                            .value
                            .as_str()
                            .context("reasoning config value must be a string")?
                            .to_string(),
                    )),
                    "policy_mode" => {
                        SessionConfigUpdate::Mode(serde_json::from_value(params.value)?)
                    }
                    other => anyhow::bail!("unknown session config option: {other}"),
                };
                self.service.set_config(&id, update).await?;
                let snapshot = self.service.snapshot(&id).await?;
                Ok(json!({
                    "updated": true,
                    "usage": snapshot.usage,
                }))
            }
            "session.options" => {
                let id = parse_session(params)?;
                let record = self.service.snapshot(&id).await?.record;
                Ok(serde_json::to_value(super::SessionOptionSnapshot {
                    config: record.config(),
                    models: self
                        .profile
                        .read()
                        .expect("profile lock poisoned")
                        .model_options
                        .clone(),
                })?)
            }
            "session.permission" => {
                let params: PermissionParam = serde_json::from_value(params)?;
                let id = SessionId::parse(params.session_id).map_err(anyhow::Error::msg)?;
                let endpoint = EndpointId::parse(params.endpoint_id).map_err(anyhow::Error::msg)?;
                self.service
                    .respond_permission(
                        &id,
                        endpoint,
                        params.request_id,
                        ConfirmationDecision {
                            allowed: params.allowed,
                            reason: params.reason,
                        },
                    )
                    .await?;
                Ok(json!({"resolved": true}))
            }
            other => anyhow::bail!("unknown session method: {other}"),
        }
    }

    pub(super) async fn resolve_prompt_session(
        &self,
        params: &PromptParam,
        caller: Option<SessionId>,
    ) -> Result<(SessionId, Option<SessionId>)> {
        anyhow::ensure!(
            params.session_id.is_none() || params.from_session_id.is_none(),
            "--from cannot be used with --to"
        );
        let (default_model, default_reasoning, default_mode) = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .defaults();
        let caller_record = if let Some(id) = &caller {
            Some(self.service.snapshot(id).await?.record)
        } else {
            None
        };

        if let Some(target) = &params.session_id {
            anyhow::ensure!(
                params.title.is_none() && params.cwd.is_none(),
                "--title and --cwd can only be used when creating a subsession"
            );
            anyhow::ensure!(
                !params.ephemeral,
                "--ephemeral can only be used when creating a new session"
            );
            let id = SessionId::parse(target.clone()).map_err(anyhow::Error::msg)?;
            let record = self.service.snapshot(&id).await?.record;
            if let Some(caller) = &caller {
                anyhow::ensure!(
                    record.info.parent_session_id.as_ref() == Some(caller),
                    "session {id} is not a direct subsession of {caller}"
                );
            }
            if let (Some(parent), Some(mode)) = (&caller_record, params.policy) {
                ensure_policy_ceiling(mode, parent.info.mode)?;
            }
            apply_prompt_config(&self.service, &id, params).await?;
            return Ok((id, record.info.parent_session_id.clone()));
        }

        if let Some(source) = &params.from_session_id {
            anyhow::ensure!(
                params.cwd.is_none(),
                "--cwd cannot be used when forking a session"
            );
            anyhow::ensure!(
                !params.ephemeral,
                "--ephemeral can only be used when creating a new session"
            );
            let source_id = SessionId::parse(source.clone()).map_err(anyhow::Error::msg)?;
            let source_record = self.service.snapshot(&source_id).await?.record;
            if let Some(caller) = &caller {
                anyhow::ensure!(
                    source_record.info.parent_session_id.as_ref() == Some(caller),
                    "session {source_id} is not a direct subsession of {caller}"
                );
            }
            let mode = params.policy.unwrap_or(source_record.info.mode);
            if let Some(parent) = &caller_record {
                ensure_policy_ceiling(mode, parent.info.mode)?;
            }
            let parent_id = source_record.info.parent_session_id.clone();
            let id = self
                .create_session(HostSessionOptions {
                    from: Some(source_id),
                    parent_session_id: caller,
                    title: params.title.clone(),
                    ..HostSessionOptions::default()
                })
                .await?;
            if let Err(error) = apply_prompt_config(&self.service, &id, params).await {
                let _ = self.service.delete(&id).await;
                return Err(error.into());
            }
            return Ok((id, parent_id));
        }

        let inherited_mode = caller_record
            .as_ref()
            .map_or(default_mode, |record| record.info.mode);
        let mode = params.policy.unwrap_or(inherited_mode);
        if let Some(parent) = &caller_record {
            ensure_policy_ceiling(mode, parent.info.mode)?;
        }
        let model = params.model.clone().unwrap_or_else(|| {
            caller_record
                .as_ref()
                .map_or_else(|| default_model.clone(), |record| record.llm.model.clone())
        });
        let reasoning = params
            .reasoning
            .clone()
            .or_else(|| {
                caller_record
                    .as_ref()
                    .and_then(|record| record.llm.reasoning.clone())
            })
            .or_else(|| {
                (caller_record.is_none() && params.model.is_none())
                    .then(|| default_reasoning.clone())
                    .flatten()
            });
        let requested_cwd = params
            .cwd
            .clone()
            .or_else(|| caller_record.as_ref().map(|record| record.info.cwd.clone()));
        let inherited_topic = if params.cwd.is_none() {
            caller
                .as_ref()
                .and_then(|id| self.projects.locate_session(id.as_str()))
        } else {
            None
        };
        let (project_id, topic_id, cwd) = match inherited_topic {
            Some((project, topic)) => (Some(project.id), Some(topic.id), None),
            None => (None, None, requested_cwd),
        };
        let id = self
            .create_session(HostSessionOptions {
                title: params.title.clone(),
                cwd,
                project_id,
                topic_id,
                parent_session_id: caller.clone(),
                mode: Some(mode),
                llm: Some(SessionLlmSettings::new(model, reasoning)),
                ephemeral: params.ephemeral,
                ..HostSessionOptions::default()
            })
            .await?;
        Ok((id, caller))
    }

    pub async fn subscribe_session(
        &self,
        id: &SessionId,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription> {
        Ok(self.service.subscribe(id, checkpoint_cursor).await?)
    }

    pub async fn prompt_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted> {
        let snapshot = self.service.snapshot(id).await?;
        let content = self
            .expand_prompt_directives(&snapshot.record.info.cwd, content)
            .await?;
        Ok(self.service.prompt(id, endpoint, content).await?)
    }

    pub(crate) async fn expand_prompt_directives(
        &self,
        cwd: &Path,
        content: MessageContent,
    ) -> Result<MessageContent> {
        let kinds: DirectiveKinds = directive_kinds(&content);
        if kinds.is_empty() {
            return Ok(content);
        }
        let skills = if kinds.skill {
            let external = self
                .profile
                .read()
                .expect("profile lock poisoned")
                .config
                .external_skills_dirs
                .clone();
            super::skill_api::skill_snapshots(&self.profile_root, &external, cwd)?
                .into_iter()
                .map(|skill| AvailableSkill {
                    name: skill.name,
                    path: skill.path,
                })
                .collect()
        } else {
            Vec::new()
        };
        let mcp_servers = if kinds.mcp {
            self.mcp
                .catalog_snapshot()
                .await?
                .servers
                .into_iter()
                .map(|server| server.name)
                .collect()
        } else {
            Vec::new()
        };
        Ok(expand_prompt_directives(content, &skills, &mcp_servers))
    }

    pub(crate) async fn prompt_directive_options(&self, id: &SessionId) -> Result<Value> {
        let snapshot = self.service.snapshot(id).await?;
        let external = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .config
            .external_skills_dirs
            .clone();
        let skills = super::skill_api::skill_snapshots(
            &self.profile_root,
            &external,
            &snapshot.record.info.cwd,
        )?
        .into_iter()
        .filter(|skill| !skill.name.chars().any(char::is_whitespace))
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
            })
        })
        .collect::<Vec<_>>();
        let mcp_servers = self
            .mcp
            .catalog_snapshot()
            .await?
            .servers
            .into_iter()
            .filter(|server| !server.name.chars().any(char::is_whitespace))
            .map(|server| {
                json!({
                    "name": server.name,
                    "description": server.description,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "skills": skills,
            "mcpServers": mcp_servers,
        }))
    }
}

pub(super) fn parse_session(params: Value) -> Result<SessionId> {
    let params: SessionIdParam = serde_json::from_value(params)?;
    SessionId::parse(params.session_id).map_err(anyhow::Error::msg)
}

pub(super) fn parse_optional_session(value: Option<String>) -> Result<Option<SessionId>> {
    value
        .map(SessionId::parse)
        .transpose()
        .map_err(anyhow::Error::msg)
}

async fn apply_prompt_config(
    service: &SessionService,
    id: &SessionId,
    params: &PromptParam,
) -> std::result::Result<(), dwo_agent_service::SessionServiceError> {
    if let Some(mode) = params.policy {
        service
            .set_config(id, SessionConfigUpdate::Mode(mode))
            .await?;
    }
    if let Some(model) = &params.model {
        service
            .set_config(id, SessionConfigUpdate::Model(model.clone()))
            .await?;
    }
    if let Some(reasoning) = &params.reasoning {
        service
            .set_config(id, SessionConfigUpdate::Reasoning(Some(reasoning.clone())))
            .await?;
    }
    Ok(())
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

fn is_content_event(payload: &SessionEventPayload) -> bool {
    matches!(
        payload,
        SessionEventPayload::UserPromptSubmitted { .. }
            | SessionEventPayload::AssistantCompleted { .. }
            | SessionEventPayload::AssistantInterrupted { .. }
            | SessionEventPayload::Notification { .. }
            | SessionEventPayload::ToolStarted { .. }
            | SessionEventPayload::ToolUpdated { .. }
            | SessionEventPayload::ToolCompleted { .. }
            | SessionEventPayload::TerminalOpened { .. }
            | SessionEventPayload::TerminalExited { .. }
            | SessionEventPayload::FileRead { .. }
            | SessionEventPayload::FileChanged { .. }
            | SessionEventPayload::PermissionRequested { .. }
            | SessionEventPayload::PermissionResolved { .. }
            | SessionEventPayload::PlanUpdated { .. }
            | SessionEventPayload::TurnCancelled { .. }
            | SessionEventPayload::TurnFailed { .. }
    )
}

fn spawn_result_delivery(
    service: Arc<SessionService>,
    mut subscription: SessionSubscription,
    child_id: SessionId,
    parent_id: SessionId,
    watched_turn: dwo_agent_service::TurnId,
) {
    tokio::spawn(async move {
        let mut content = String::new();
        let (status, error) = loop {
            let Some(event) = subscription.events.recv().await else {
                break ("closed", None);
            };
            match event.payload {
                SessionEventPayload::AssistantCompleted {
                    turn_id,
                    content: completed,
                    ..
                } if turn_id == watched_turn => content = completed,
                SessionEventPayload::TurnCompleted { turn_id } if turn_id == watched_turn => {
                    break ("completed", None);
                }
                SessionEventPayload::TurnCancelled { turn_id } if turn_id == watched_turn => {
                    break ("cancelled", None);
                }
                SessionEventPayload::TurnFailed { turn_id, error } if turn_id == watched_turn => {
                    break ("failed", Some(error));
                }
                _ => {}
            }
        };
        let notification = format!(
            "<subsession_result>\n{}\n</subsession_result>",
            json!({
                "session_id": child_id,
                "status": status,
                "content": content,
                "error": error,
            })
        );
        let parent_subscription = match service.subscribe(&parent_id, None).await {
            Ok(subscription) => subscription,
            Err(error) => {
                tracing::error!(
                    event = "subsession.parent_observe_failed",
                    parent_session_id = %parent_id,
                    error = %format!("{error:#}"),
                    "observe subsession parent failed"
                );
                return;
            }
        };
        let grandparent_id = parent_subscription
            .snapshot
            .record
            .info
            .parent_session_id
            .clone();
        match service
            .prompt_internal(&parent_id, MessageContent::text(notification))
            .await
        {
            Ok(parent_turn) => {
                if let Some(grandparent_id) = grandparent_id {
                    spawn_result_delivery(
                        service,
                        parent_subscription,
                        parent_id,
                        grandparent_id,
                        parent_turn,
                    );
                }
            }
            Err(error) => tracing::error!(
                event = "subsession.result_delivery_failed",
                parent_session_id = %parent_id,
                error = %format!("{error:#}"),
                "deliver subsession result failed"
            ),
        }
    });
}

async fn cleanup_deleted_session_resources(profile_root: &Path, id: &SessionId) -> Result<()> {
    for channel in ["weixin", "telegram", "feishu"] {
        remove_session_attachment_dirs(
            &profile_root.join("runtime/attachments").join(channel),
            id.as_str(),
        )
        .await?;
    }
    Ok(())
}

async fn remove_session_attachment_dirs(root: &Path, session_id: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut entries = tokio::fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if !file_type.is_dir() {
                continue;
            }
            if entry.file_name() == std::ffi::OsStr::new(session_id) {
                tokio::fs::remove_dir_all(entry.path()).await?;
            } else {
                directories.push(entry.path());
            }
        }
    }
    Ok(())
}
