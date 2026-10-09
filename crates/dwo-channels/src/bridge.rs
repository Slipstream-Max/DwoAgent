use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use dwo_agent_service::{
    ActiveToolCall, EndpointId, MessageContent, PendingPermission, SessionConfigUpdate,
    SessionEventPayload, SessionId,
};
use dwo_tools::{ConfirmationDecision, SessionMode};
use tokio::sync::Mutex;

use super::{ChannelHost, SessionCreateRequest};
use dwo_command::{ChannelCommand, render_command_help};

use super::render::{
    SessionStreamState, display_path, policy_name, render_questionnaire, render_session_replay,
    render_status, render_tool_call, render_user_prompt_segments, short_session_id,
    short_session_id_str,
};

#[async_trait]
pub trait ChannelIngress: Send + Sync {
    async fn execute(&self, command: ChannelCommand) -> Result<Vec<String>>;
    async fn ensure_prompt_session(&self) -> Result<SessionId>;
    async fn submit_prompt(&self, content: MessageContent) -> Result<()>;
    async fn resolve_permission(
        &self,
        session_id: &SessionId,
        request_id: &str,
        allowed: bool,
    ) -> Result<()>;
}

#[async_trait]
pub trait ConversationTransport: Send + Sync {
    async fn send_text(&self, text: &str) -> Result<()>;
    async fn send_permission_request(
        &self,
        _session_id: &SessionId,
        call: &ActiveToolCall,
        permission: &PendingPermission,
    ) -> Result<()> {
        self.send_text(&render_tool_call(
            call,
            "request id",
            &permission.request_id,
        ))
        .await
    }
    async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConversationId {
    channel: String,
    peer_id: String,
}

impl ConversationId {
    pub fn new(channel: impl Into<String>, peer_id: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            peer_id: peer_id.into(),
        }
    }

    fn endpoint(&self) -> EndpointId {
        let safe = format!("{}-{}", self.channel, self.peer_id)
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        EndpointId::parse(safe).expect("sanitized channel endpoint")
    }

    fn denial_source(&self) -> &str {
        match self.channel.as_str() {
            "weixin" => "Weixin",
            other => other,
        }
    }
}

struct SessionObserver {
    session_id: String,
    task: tokio::task::JoinHandle<()>,
}

pub struct SessionBridge {
    host: Arc<dyn ChannelHost>,
    conversation: ConversationId,
    endpoint: EndpointId,
    replay_turns: usize,
    selected_session_id: Mutex<Option<String>>,
    session_choices: Mutex<HashMap<Option<String>, Vec<super::ChannelSessionItem>>>,
    transport: Arc<dyn ConversationTransport>,
    observer: Mutex<Option<SessionObserver>>,
}

impl SessionBridge {
    pub fn new(
        host: Arc<dyn ChannelHost>,
        conversation: ConversationId,
        replay_turns: usize,
        selected_session_id: Option<String>,
        transport: Arc<dyn ConversationTransport>,
    ) -> Self {
        let endpoint = conversation.endpoint();
        Self {
            host,
            conversation,
            endpoint,
            replay_turns,
            selected_session_id: Mutex::new(selected_session_id),
            session_choices: Mutex::new(HashMap::new()),
            transport,
            observer: Mutex::new(None),
        }
    }

    pub async fn execute(&self, command: ChannelCommand) -> Result<Vec<String>> {
        let messages = match command {
            ChannelCommand::Help => vec![render_command_help()],
            ChannelCommand::List { scope, page } => {
                let (project, page) = list_scope(scope, page)?;
                self.list_session_messages(project, page).await?
            }
            ChannelCommand::New { project, section } => {
                let snapshot = self
                    .host
                    .create_session(SessionCreateRequest {
                        project,
                        section,
                        ..SessionCreateRequest::default()
                    })
                    .await?;
                let session_id = snapshot.record.info.id.clone();
                self.select_session(session_id.as_str()).await?;
                vec![format!(
                    "Selected new session {}\nCwd: {}",
                    session_id,
                    display_path(&snapshot.record.info.cwd)
                )]
            }
            ChannelCommand::Fork => {
                let source_id = self.selected_session_id().await?;
                let snapshot = self
                    .host
                    .create_session(SessionCreateRequest {
                        from: Some(source_id),
                        ..SessionCreateRequest::default()
                    })
                    .await?;
                vec![format!(
                    "Forked session {}\nTitle: {}",
                    snapshot.record.info.id, snapshot.record.info.title
                )]
            }
            ChannelCommand::Use { session: None, .. } => {
                let mut messages = self.list_session_messages(None, 1).await?;
                if let Some(first) = messages.first_mut() {
                    *first = format!("选择会话：发送 /use <编号> 或 /use <短 ID>\n\n{first}");
                }
                messages
            }
            ChannelCommand::Use {
                session: Some(reference),
                number,
            } => {
                let id = if let Some(number) = number {
                    self.resolve_list_number(Some(&reference), number).await?
                } else {
                    self.resolve_session_reference(&reference).await?
                };
                let session_id = SessionId::parse(id.clone()).map_err(anyhow::Error::msg)?;
                self.select_session(&id).await?;
                let subscription = self
                    .host
                    .subscribe_session(&session_id, self.endpoint.clone(), None)
                    .await?;
                let replay = render_session_replay(&subscription.snapshot, self.replay_turns);
                if replay.is_empty() {
                    vec![render_status(&subscription.snapshot)]
                } else {
                    replay
                }
            }
            ChannelCommand::Status => {
                let snapshot = self
                    .host
                    .session_snapshot(&self.selected_session_id().await?)
                    .await?;
                vec![render_status(&snapshot)]
            }
            ChannelCommand::Cancel => {
                self.host
                    .cancel_session(&self.selected_session_id().await?, None)
                    .await?;
                vec!["Cancellation requested".to_string()]
            }
            ChannelCommand::Compact => {
                let id = self.selected_session_id().await?;
                self.ensure_observer(&id).await?;
                self.host
                    .compact_session(&id, self.endpoint.clone())
                    .await?;
                Vec::new()
            }
            ChannelCommand::Resume => {
                let id = self.selected_session_id().await?;
                self.ensure_observer(&id).await?;
                let _ = self
                    .host
                    .prompt_internal(&id, MessageContent::text(""))
                    .await?;
                Vec::new()
            }
            ChannelCommand::Model { name } => {
                let id = self.selected_session_id().await?;
                let options = self.host.session_options(&id).await?;
                if let Some(name) = name {
                    let model = resolve_model(&options.models, &name)?;
                    self.host
                        .set_session_config(&id, SessionConfigUpdate::Model(model.id.clone()))
                        .await?;
                    vec![format!("Model: {}", model.id)]
                } else {
                    vec![render_models(&options)]
                }
            }
            ChannelCommand::Think { level } => {
                let id = self.selected_session_id().await?;
                let options = self.host.session_options(&id).await?;
                let model = options
                    .models
                    .iter()
                    .find(|m| m.id == options.config.model)
                    .context("Current model is unavailable")?;
                if let Some(level) = level {
                    anyhow::ensure!(
                        model.reasoning.iter().any(|r| r.id == level),
                        "Unsupported reasoning '{level}' for {}",
                        model.id
                    );
                    self.host
                        .set_session_config(&id, SessionConfigUpdate::Reasoning(Some(level)))
                        .await?;
                    let current = self.host.session_options(&id).await?;
                    vec![format!(
                        "Reasoning: {}",
                        current
                            .config
                            .reasoning
                            .as_deref()
                            .unwrap_or(&model.default_reasoning)
                    )]
                } else {
                    vec![format!(
                        "Model: {}\nReasoning: {}\nSupported: {}",
                        model.id,
                        options
                            .config
                            .reasoning
                            .as_deref()
                            .unwrap_or(&model.default_reasoning),
                        if model.reasoning.is_empty() {
                            "(none)".to_string()
                        } else {
                            model
                                .reasoning
                                .iter()
                                .map(|r| r.id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    )]
                }
            }
            ChannelCommand::Skill => {
                let selected = self.selected_session_id.lock().await.clone();
                let session_id = selected
                    .map(|id| SessionId::parse(id).map_err(anyhow::Error::msg))
                    .transpose()?;
                let skills = self.host.list_skills(session_id.as_ref()).await?;
                if skills.is_empty() {
                    vec!["No skills available".to_string()]
                } else {
                    let mut lines = Vec::with_capacity(skills.len() + 2);
                    lines.push(format!("Skills ({})", skills.len()));
                    for (index, (name, description)) in skills.iter().enumerate() {
                        lines.push(format!(
                            "{}. {}: {}",
                            index + 1,
                            name,
                            short_skill_description(description)
                        ));
                    }
                    lines.push("Use: /skill <name> [prompt]".to_string());
                    vec![lines.join("\n")]
                }
            }
            ChannelCommand::Policy { mode } => {
                let id = self.selected_session_id().await?;
                if let Some(value) = mode {
                    let mode = SessionMode::parse(&value).map_err(anyhow::Error::msg)?;
                    self.host
                        .set_session_config(&id, SessionConfigUpdate::Mode(mode))
                        .await?;
                    vec![format!("Policy updated: {}", policy_name(mode))]
                } else {
                    let snapshot = self.host.session_snapshot(&id).await?;
                    vec![format!(
                        "Policy: {}\nOptions: full_access | confirm | watch",
                        policy_name(snapshot.record.info.mode)
                    )]
                }
            }
            ChannelCommand::Allow { id } => {
                let session_id = self.selected_session_id().await?;
                let request_id = self.permission_request(id).await?;
                self.host
                    .resolve_session_permission(
                        &session_id,
                        self.endpoint.clone(),
                        request_id,
                        ConfirmationDecision {
                            allowed: true,
                            reason: None,
                        },
                    )
                    .await?;
                Vec::new()
            }
            ChannelCommand::Deny { id } => {
                let session_id = self.selected_session_id().await?;
                let request_id = self.permission_request(id).await?;
                self.host
                    .resolve_session_permission(
                        &session_id,
                        self.endpoint.clone(),
                        request_id,
                        ConfirmationDecision {
                            allowed: false,
                            reason: Some(format!(
                                "denied from {}",
                                self.conversation.denial_source()
                            )),
                        },
                    )
                    .await?;
                Vec::new()
            }
        };
        Ok(messages)
    }

    pub async fn submit_prompt(&self, content: MessageContent) -> Result<()> {
        let session_id = self.ensure_prompt_session().await?;
        self.ensure_observer(&session_id).await?;
        self.host
            .prompt_session(&session_id, self.endpoint.clone(), content)
            .await?;
        Ok(())
    }

    pub async fn resolve_permission(
        &self,
        session_id: &SessionId,
        request_id: &str,
        allowed: bool,
    ) -> Result<()> {
        self.host
            .resolve_session_permission(
                session_id,
                self.endpoint.clone(),
                request_id.to_string(),
                ConfirmationDecision {
                    allowed,
                    reason: (!allowed)
                        .then(|| format!("denied from {}", self.conversation.denial_source())),
                },
            )
            .await
    }

    pub async fn ensure_prompt_session(&self) -> Result<SessionId> {
        if let Some(id) = self.selected_session_id.lock().await.clone() {
            return SessionId::parse(id).map_err(anyhow::Error::msg);
        }
        let session_id = {
            let snapshot = self
                .host
                .create_session(SessionCreateRequest::default())
                .await?;
            let session_id = snapshot.record.info.id;
            self.select_session(session_id.as_str()).await?;
            session_id
        };
        Ok(session_id)
    }

    pub async fn resume_observer(&self) -> Result<()> {
        let Some(id) = self.selected_session_id.lock().await.clone() else {
            return Ok(());
        };
        let session_id = SessionId::parse(id).map_err(anyhow::Error::msg)?;
        self.ensure_observer(&session_id).await
    }

    pub async fn stop(&self) {
        if let Some(observer) = self.observer.lock().await.take() {
            observer.task.abort();
        }
    }

    async fn list_session_messages(
        &self,
        project: Option<String>,
        page: usize,
    ) -> Result<Vec<String>> {
        let cached = self.session_choices.lock().await.get(&project).cloned();
        let records = if page == 1 || cached.is_none() {
            self.host.list_channel_sessions(project.as_deref()).await?
        } else {
            cached.unwrap()
        };
        let selected = self.selected_session_id.lock().await.clone();
        let rendered =
            render_session_list(&records, project.as_deref(), page, selected.as_deref())?;
        self.session_choices.lock().await.insert(project, records);
        Ok(vec![rendered])
    }

    async fn selected_session_id(&self) -> Result<SessionId> {
        let id = self
            .selected_session_id
            .lock()
            .await
            .clone()
            .context("No session selected. Use /new or /use")?;
        SessionId::parse(id).map_err(anyhow::Error::msg)
    }

    async fn resolve_session_reference(&self, reference: &str) -> Result<String> {
        let reference = reference.trim();
        if reference.is_empty() {
            anyhow::bail!("Session reference must not be empty");
        }
        if let Ok(index) = reference.parse::<usize>() {
            return self.resolve_list_number(None, index).await;
        }
        if let Ok(id) = SessionId::parse(reference.to_string()) {
            return Ok(id.to_string());
        }

        let records = self.host.list_sessions(true, None).await?;
        let matches = records
            .into_iter()
            .filter(|record| session_id_matches(&record.session_id, reference))
            .map(|record| record.session_id.to_string())
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => anyhow::bail!(
                "No session matches '{reference}'. Send /list, then use its number or short ID"
            ),
            _ => anyhow::bail!(
                "Session reference '{reference}' is ambiguous: {}",
                matches
                    .iter()
                    .map(|id| short_session_id_str(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    async fn resolve_list_number(&self, project: Option<&str>, number: usize) -> Result<String> {
        let key = project.map(str::to_string);
        let cached = self.session_choices.lock().await.get(&key).cloned();
        let rows = match cached {
            Some(rows) => rows,
            // 没有快照时按当前列表实时解析：Inbox 是唯一默认列表，不必先 /list。
            None => {
                let rows = self.host.list_channel_sessions(project).await?;
                self.session_choices.lock().await.insert(key, rows.clone());
                rows
            }
        };
        number
            .checked_sub(1)
            .and_then(|index| rows.get(index))
            .map(|item| item.session.session_id.to_string())
            .with_context(|| format!("No session #{number} in the list; send /list to see it"))
    }

    async fn permission_request(&self, requested: Option<String>) -> Result<String> {
        let id = if requested.is_some() {
            resolve_permission_request_id(requested, None)?
        } else {
            let snapshot = self
                .host
                .session_snapshot(&self.selected_session_id().await?)
                .await?;
            resolve_permission_request_id(None, snapshot.pending_permission.as_ref())?
        };
        Ok(id)
    }

    async fn select_session(&self, id: &str) -> Result<()> {
        self.set_selected_session(Some(id)).await?;
        let session_id = SessionId::parse(id.to_string()).map_err(anyhow::Error::msg)?;
        self.ensure_observer(&session_id).await
    }

    async fn set_selected_session(&self, id: Option<&str>) -> Result<()> {
        *self.selected_session_id.lock().await = id.map(str::to_string);
        self.transport.save_selected_session(id).await
    }

    async fn ensure_observer(&self, id: &SessionId) -> Result<()> {
        let session_id = id.to_string();
        let mut observer = self.observer.lock().await;
        if observer
            .as_ref()
            .is_some_and(|current| current.session_id == session_id && !current.task.is_finished())
        {
            return Ok(());
        }
        if let Some(current) = observer.take() {
            current.task.abort();
        }
        let subscription = self
            .host
            .subscribe_session(id, self.endpoint.clone(), None)
            .await?;
        let transport = self.transport.clone();
        let task = tokio::spawn(stream_session(
            transport,
            self.endpoint.clone(),
            id.clone(),
            subscription,
        ));
        *observer = Some(SessionObserver { session_id, task });
        Ok(())
    }
}

fn list_scope(scope: Option<String>, page: Option<usize>) -> Result<(Option<String>, usize)> {
    let (project, page) = match (scope, page) {
        (None, None) => (None, 1),
        (Some(scope), None) => match scope.parse::<usize>() {
            Ok(page) => (None, page),
            Err(_) => (Some(scope), 1),
        },
        (Some(project), Some(page)) => (Some(project), page),
        (None, Some(_)) => anyhow::bail!("Page requires a scope"),
    };
    anyhow::ensure!(page > 0, "Page numbers start at 1");
    Ok((project, page))
}

fn render_session_list(
    records: &[super::ChannelSessionItem],
    project: Option<&str>,
    page: usize,
    selected: Option<&str>,
) -> Result<String> {
    const PAGE_SIZE: usize = 20;
    let pages = records.len().div_ceil(PAGE_SIZE).max(1);
    anyhow::ensure!(page <= pages, "Page {page} does not exist (1..={pages})");
    let mut lines = vec![format!(
        "{} — {page}/{pages}",
        project.unwrap_or("Inbox")
    )];
    let mut section = None;
    let mut group = None;
    for (index, item) in records
        .iter()
        .enumerate()
        .skip((page - 1) * PAGE_SIZE)
        .take(PAGE_SIZE)
    {
        if project.is_some() {
            if item.section != section {
                section = item.section.clone();
                if let Some(name) = &section {
                    lines.push(format!("[{name}]"));
                }
            }
        } else if group != Some(item.working) {
            group = Some(item.working);
            lines.push(if item.working { "进行中" } else { "已完成" }.to_string());
        }
        let record = &item.session;
        lines.push(format!(
            "{} {}. {} [{}]",
            if selected == Some(record.session_id.as_str()) {
                "*"
            } else {
                " "
            },
            index + 1,
            short_session_title(&record.title),
            short_session_id(&record.session_id)
        ));
    }
    if records.is_empty() {
        lines.push("No sessions".into());
    }
    if page < pages {
        lines.push(match project {
            Some(name) => format!("Next: /list \"{name}\" {}", page + 1),
            None => format!("Next: /list {}", page + 1),
        });
    }
    Ok(lines.join("\n"))
}

fn resolve_model<'a>(
    models: &'a [dwo_protocol::SessionModelOption],
    name: &str,
) -> Result<&'a dwo_protocol::SessionModelOption> {
    // A canonical provider/model ID is always an exact selection.
    if let Some(model) = models.iter().find(|model| model.id == name) {
        return Ok(model);
    }
    let matches: Vec<_> = models
        .iter()
        .filter(|m| {
            format!("{}/{}", m.provider, m.name) == name
                || m.name == name
                || m.id.strip_prefix(&format!("{}/", m.provider)) == Some(name)
        })
        .collect();
    match matches.as_slice() {
        [model] => Ok(model),
        [] => anyhow::bail!("Unknown model '{name}'; use /model to list models"),
        _ => anyhow::bail!(
            "Ambiguous model '{name}'; specify provider/model: {}",
            matches
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn render_models(options: &dwo_protocol::SessionOptions) -> String {
    let mut models: Vec<_> = options.models.iter().collect();
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    let mut lines = vec![format!("Current model: {}", options.config.model)];
    let mut provider = None;
    for model in models {
        if provider != Some(&model.provider) {
            provider = Some(&model.provider);
            lines.push(format!("[{}]", model.provider));
        }
        lines.push(format!(
            "{} {} ({})",
            if model.id == options.config.model {
                "*"
            } else {
                " "
            },
            model.id,
            model.name
        ));
    }
    lines.join("\n")
}

fn short_session_title(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        return "Untitled".to_string();
    }
    let mut shortened = title.chars().take(32).collect::<String>();
    if title.chars().count() > 32 {
        shortened.push_str("...");
    }
    shortened
}

fn short_skill_description(description: &str) -> String {
    let description = description.lines().next().unwrap_or("").trim();
    if description.is_empty() {
        return "(no description)".to_string();
    }
    let mut shortened = description.chars().take(72).collect::<String>();
    if description.chars().count() > 72 {
        shortened.push_str("...");
    }
    shortened
}

fn session_id_matches(id: &SessionId, reference: &str) -> bool {
    let id = id.as_str();
    id.starts_with(reference)
        || id
            .strip_prefix("session-")
            .is_some_and(|short_id| short_id.starts_with(reference))
}

#[cfg(test)]
mod session_reference_tests {
    use super::*;

    #[test]
    fn list_pages_require_positive_numbers_and_keep_project_scope() {
        assert_eq!(list_scope(None, None).unwrap(), (None, 1));
        assert_eq!(list_scope(Some("2".into()), None).unwrap(), (None, 2));
        assert_eq!(
            list_scope(Some("Project".into()), Some(2)).unwrap(),
            (Some("Project".into()), 2)
        );
        assert!(list_scope(Some("0".into()), None).is_err());
    }

    #[test]
    fn duplicate_model_names_require_provider_qualification() {
        let models: Vec<dwo_protocol::SessionModelOption> = serde_json::from_value(serde_json::json!([
            {"id":"a/model", "name":"Shared", "provider":"a", "reasoning":[], "defaultReasoning":"auto"},
            {"id":"b/model", "name":"Shared", "provider":"b", "reasoning":[], "defaultReasoning":"auto"}
        ])).unwrap();
        assert!(resolve_model(&models, "Shared").is_err());
        assert!(resolve_model(&models, "model").is_err());
        assert_eq!(resolve_model(&models, "a/model").unwrap().id, "a/model");
        assert_eq!(resolve_model(&models, "b/Shared").unwrap().id, "b/model");
        assert!(resolve_model(&models, "missing").is_err());
    }

    #[test]
    fn short_ids_are_copyable_without_the_session_prefix() {
        let id = SessionId::parse("session-1234567890abcdef").unwrap();
        assert_eq!(short_session_id(&id), "12345678");
        assert!(session_id_matches(&id, "12345678"));
        assert!(session_id_matches(&id, "session-123456"));
        assert!(!session_id_matches(&id, "abcdef"));
    }
}

#[cfg(test)]
mod channel_list_tests {
    use super::*;
    use crate::ChannelSessionItem;
    use dwo_agent_service::{RuntimePhase, SessionListItem};
    use dwo_tools::SessionMode;
    use std::path::PathBuf;

    fn item(index: usize, title: &str, working: bool) -> ChannelSessionItem {
        ChannelSessionItem {
            session: SessionListItem {
                session_id: SessionId::parse(format!("session-{index:08}")).unwrap(),
                cwd: PathBuf::from("C:/workspace"),
                title: title.to_string(),
                updated_at: 1_000 + index as u64,
                status: if working {
                    RuntimePhase::Running
                } else {
                    RuntimePhase::Idle
                },
                model: "provider/model".to_string(),
                reasoning: None,
                policy: SessionMode::Watch,
                parent_session_id: None,
                forked_from: None,
                last_turn_finished_at_ms: Some(900 + index as u64),
            },
            section: None,
            working,
        }
    }

    #[test]
    fn inbox_lists_working_sessions_first_with_group_headers() {
        let records = vec![
            item(1, "Work A", true),
            item(2, "Work B", true),
            item(3, "Done C", false),
        ];
        let text = render_session_list(&records, None, 1, Some("session-00000002")).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "Inbox — 1/1");
        assert_eq!(lines[1], "进行中");
        assert_eq!(lines[2], "  1. Work A [00000001]");
        assert_eq!(lines[3], "* 2. Work B [00000002]");
        assert_eq!(lines[4], "已完成");
        assert_eq!(lines[5], "  3. Done C [00000003]");
    }

    #[test]
    fn inbox_pages_report_their_next_page() {
        let records: Vec<_> = (0..25)
            .map(|i| item(i + 1, &format!("S{}", i + 1), false))
            .collect();
        let page_one = render_session_list(&records, None, 1, None).unwrap();
        assert!(page_one.contains("Inbox — 1/2"));
        assert!(page_one.contains("20. S20 [00000020]"));
        assert!(!page_one.contains("21. S21"));
        assert!(page_one.ends_with("Next: /list 2"));
        let page_two = render_session_list(&records, None, 2, None).unwrap();
        assert!(page_two.contains("Inbox — 2/2"));
        assert!(page_two.contains("21. S21 [00000021]"));
        assert!(!page_two.contains("Next: /list"));
        assert!(render_session_list(&records, None, 3, None).is_err());
    }

    #[test]
    fn project_scope_keeps_section_headers() {
        let mut records = vec![item(1, "A", false), item(2, "B", false)];
        records[0].section = Some("Development".to_string());
        let text = render_session_list(&records, Some("My project"), 1, None).unwrap();
        assert!(text.starts_with("My project — 1/1\n"));
        assert!(text.contains("[Development]"));
        assert!(!text.contains("进行中"));
    }

    #[test]
    fn empty_inbox_reports_no_sessions() {
        let text = render_session_list(&[], None, 1, None).unwrap();
        assert!(text.contains("No sessions"));
    }
}

#[async_trait]
impl ChannelIngress for SessionBridge {
    async fn execute(&self, command: ChannelCommand) -> Result<Vec<String>> {
        SessionBridge::execute(self, command).await
    }

    async fn ensure_prompt_session(&self) -> Result<SessionId> {
        SessionBridge::ensure_prompt_session(self).await
    }

    async fn submit_prompt(&self, content: MessageContent) -> Result<()> {
        SessionBridge::submit_prompt(self, content).await
    }

    async fn resolve_permission(
        &self,
        session_id: &SessionId,
        request_id: &str,
        allowed: bool,
    ) -> Result<()> {
        SessionBridge::resolve_permission(self, session_id, request_id, allowed).await
    }
}

fn resolve_permission_request_id(
    requested: Option<String>,
    pending: Option<&PendingPermission>,
) -> Result<String> {
    requested
        .or_else(|| pending.map(|permission| permission.request_id.clone()))
        .context("No pending permission request")
}

async fn stream_session(
    transport: Arc<dyn ConversationTransport>,
    endpoint: EndpointId,
    session_id: SessionId,
    mut subscription: dwo_agent_service::SessionSubscription,
) {
    let mut stream = SessionStreamState::default();
    loop {
        let Some(event) = subscription.events.recv().await else {
            break;
        };
        match event.payload {
            SessionEventPayload::UserPromptSubmitted {
                origin, content, ..
            } => {
                if origin == endpoint {
                    continue;
                }
                send_user_prompt(&transport, &content).await;
            }
            SessionEventPayload::AssistantCompleted {
                content, tool_calls, ..
            } => {
                stream.remember_response(content);
                for call in tool_calls {
                    stream.remember_tool(call);
                }
            }
            SessionEventPayload::ToolStarted { call, .. } => {
                stream.remember_tool(call);
            }
            SessionEventPayload::ToolUpdated { call, .. } => {
                stream.remember_tool(call);
            }
            SessionEventPayload::PermissionRequested { permission, .. } => {
                if !stream.mark_permission_sent(&permission.tool_call_id) {
                    continue;
                }
                let call = stream
                    .tool(&permission.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| ActiveToolCall {
                        tool_call_id: permission.tool_call_id.clone(),
                        tool_name: permission.tool_name.clone(),
                        raw_input: serde_json::Value::Null,
                        status: "pending".to_string(),
                    });
                if let Err(error) = transport
                    .send_permission_request(&session_id, &call, &permission)
                    .await
                {
                    tracing::warn!(
                        event = "channel.permission_send_failed",
                        error = %format!("{error:#}"),
                        "send permission request failed"
                    );
                }
            }
            SessionEventPayload::QuestionnaireRequested {
                questionnaire_id,
                questions,
                ..
            } => {
                // 问卷是**单向通知**：只发出去，不等回复、不判「跳过 / 取消」
                // ——用户后面回什么都是普通消息；正式作答只存在于 ACP v2。
                if stream.mark_questionnaire_sent(&questionnaire_id) {
                    send(&transport, &render_questionnaire(&questions)).await;
                }
            }
            SessionEventPayload::ToolCompleted { result, .. } => {
                stream.forget_tool(&result.tool_call_id);
            }
            SessionEventPayload::TurnCompleted { .. } => {
                if let Some(response) = stream.take_response() {
                    send(&transport, &response).await;
                }
                stream.finish_turn();
            }
            SessionEventPayload::TurnCancelled { .. } => {
                match stream.take_response() {
                    Some(response) => {
                        send(&transport, &format!("{response}\n\nTurn cancelled")).await;
                    }
                    None => send(&transport, "Turn cancelled").await,
                }
                stream.finish_turn();
            }
            SessionEventPayload::TurnFailed { error, .. } => {
                let marker = format!("Turn failed: {error}");
                match stream.take_response() {
                    Some(response) => send(&transport, &format!("{response}\n\n{marker}")).await,
                    None => send(&transport, &marker).await,
                }
                stream.finish_turn();
            }
            _ => {}
        }
    }
}

async fn send(transport: &Arc<dyn ConversationTransport>, text: &str) {
    if let Err(error) = transport.send_text(text).await {
        tracing::warn!(
            event = "channel.message_send_failed",
            error = %format!("{error:#}"),
            "send channel message failed"
        );
    }
}

async fn send_user_prompt(transport: &Arc<dyn ConversationTransport>, content: &MessageContent) {
    let mut first = true;
    for segment in render_user_prompt_segments(content) {
        let message = if first {
            first = false;
            format!("User: {segment}")
        } else {
            segment
        };
        send(transport, &message).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dwo_agent_service::{
        ClientTranscriptEvent, MessageId, RuntimePhase, SessionLlmSettings, SessionRecord,
        SessionSnapshot, SessionSubscription, SessionUsageSnapshot, TurnId,
    };
    use dwo_context::ContentBlock;
    use serde_json::json;
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeTransport {
        messages: Mutex<Vec<String>>,
        selected: Mutex<Option<String>>,
    }

    #[async_trait]
    impl ConversationTransport for FakeTransport {
        async fn send_text(&self, text: &str) -> Result<()> {
            self.messages.lock().await.push(text.to_string());
            Ok(())
        }

        async fn send_permission_request(
            &self,
            _session_id: &SessionId,
            call: &ActiveToolCall,
            _permission: &PendingPermission,
        ) -> Result<()> {
            self.messages
                .lock()
                .await
                .push(format!("CARD:{}", call.tool_call_id));
            Ok(())
        }

        async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()> {
            *self.selected.lock().await = session_id.map(str::to_string);
            Ok(())
        }
    }

    #[test]
    fn conversation_endpoint_is_stable_and_sanitized() {
        let conversation = ConversationId::new("telegram", "user +123");
        assert_eq!(conversation.endpoint().to_string(), "telegram-user--123");
    }

    #[test]
    fn permission_command_uses_the_current_pending_request_when_id_is_omitted() {
        let pending = PendingPermission {
            request_id: "permission-7".to_string(),
            tool_call_id: "tool-7".to_string(),
            tool_name: "terminal".to_string(),
        };

        assert_eq!(
            resolve_permission_request_id(None, Some(&pending)).unwrap(),
            "permission-7"
        );
        assert_eq!(
            resolve_permission_request_id(Some("permission-8".to_string()), Some(&pending))
                .unwrap(),
            "permission-8"
        );
        assert_eq!(
            resolve_permission_request_id(None, None)
                .unwrap_err()
                .to_string(),
            "No pending permission request"
        );
    }

    #[tokio::test]
    async fn session_stream_forwards_prompt_and_completed_answer_through_transport() {
        let transport = Arc::new(FakeTransport::default());
        let session_id = SessionId::parse("session-test").unwrap();
        let turn_id = TurnId::parse("turn-test").unwrap();
        let record = SessionRecord::new(
            session_id.clone(),
            "Test".to_string(),
            dwo_agent_service::SessionWorkspace::External {
                pwd: PathBuf::from("."),
            },
            PathBuf::from("."),
            SessionMode::Confirm,
            SessionLlmSettings::default(),
        );
        let (events_tx, events) = tokio::sync::mpsc::channel(8);
        let subscription = SessionSubscription {
            snapshot: SessionSnapshot {
                record,
                transcript: Vec::<ClientTranscriptEvent>::new(),
                checkpoint_cursor: 0,
                usage: SessionUsageSnapshot { used: 1, size: 2 },
                phase: RuntimePhase::Running,
                active_turn_id: Some(turn_id.clone()),
                active_step: None,
                partial_message: String::new(),
                active_tool_calls: Vec::new(),
                pending_permission: None,
                terminal_views: Vec::new(),
                live_terminal_ids: Vec::new(),
                seq: 0,
            },
            events,
        };
        let task = tokio::spawn(stream_session(
            transport.clone(),
            EndpointId::new(),
            session_id.clone(),
            subscription,
        ));
        for (seq, payload) in [
            SessionEventPayload::UserPromptSubmitted {
                message_id: MessageId::new(),
                turn_id: turn_id.clone(),
                origin: EndpointId::new(),
                content: MessageContent::text("question"),
                source: None,
            },
            SessionEventPayload::AssistantCompleted {
                message_id: MessageId::new(),
                thought_message_id: MessageId::new(),
                turn_id: turn_id.clone(),
                content: "answer".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
            },
            SessionEventPayload::TurnCompleted { turn_id },
        ]
        .into_iter()
        .enumerate()
        {
            events_tx
                .send(dwo_agent_service::SessionEvent {
                    seq: seq as u64 + 1,
                    session_id: session_id.clone(),
                    payload,
                })
                .await
                .unwrap();
        }
        drop(events_tx);
        task.await.unwrap();

        assert_eq!(
            *transport.messages.lock().await,
            ["User: question", "answer"]
        );
    }

    #[tokio::test]
    async fn session_stream_forwards_questionnaire_as_a_message() {
        let transport = Arc::new(FakeTransport::default());
        let session_id = SessionId::parse("session-test").unwrap();
        let turn_id = TurnId::parse("turn-test").unwrap();
        let record = SessionRecord::new(
            session_id.clone(),
            "Test".to_string(),
            dwo_agent_service::SessionWorkspace::External {
                pwd: PathBuf::from("."),
            },
            PathBuf::from("."),
            SessionMode::Confirm,
            SessionLlmSettings::default(),
        );
        let (events_tx, events) = tokio::sync::mpsc::channel(8);
        let subscription = SessionSubscription {
            snapshot: SessionSnapshot {
                record,
                transcript: Vec::<ClientTranscriptEvent>::new(),
                checkpoint_cursor: 0,
                usage: SessionUsageSnapshot { used: 1, size: 2 },
                phase: RuntimePhase::Running,
                active_turn_id: Some(turn_id.clone()),
                active_step: None,
                partial_message: String::new(),
                active_tool_calls: Vec::new(),
                pending_permission: None,
                terminal_views: Vec::new(),
                live_terminal_ids: Vec::new(),
                seq: 0,
            },
            events,
        };
        let task = tokio::spawn(stream_session(
            transport.clone(),
            EndpointId::new(),
            session_id.clone(),
            subscription,
        ));
        let questions = vec![dwo_tools::QuestionnaireQuestion {
            question: "继续吗？".to_string(),
            options: vec!["继续".to_string(), "停".to_string()],
            multiple: false,
        }];
        events_tx
            .send(dwo_agent_service::SessionEvent {
                seq: 1,
                session_id: session_id.clone(),
                payload: SessionEventPayload::QuestionnaireRequested {
                    questionnaire_id: "questionnaire-1".to_string(),
                    turn_id,
                    questions: questions.clone(),
                },
            })
            .await
            .unwrap();
        drop(events_tx);
        task.await.unwrap();

        assert_eq!(
            *transport.messages.lock().await,
            [render_questionnaire(&questions)]
        );
    }

    #[tokio::test]
    async fn multimodal_prompt_broadcast_keeps_block_order_with_placeholders() {
        let transport = Arc::new(FakeTransport::default());
        let content = MessageContent::blocks(vec![
            ContentBlock::text("look at this"),
            ContentBlock::image("image/png", "SECRETBYTES"),
            ContentBlock::text("done"),
        ]);

        let prompt_transport: Arc<dyn ConversationTransport> = transport.clone();
        send_user_prompt(&prompt_transport, &content).await;

        let messages = transport.messages.lock().await.clone();
        assert_eq!(messages, ["User: look at this", "[图片]", "done"]);
        assert!(
            messages
                .iter()
                .all(|message| !message.contains("SECRETBYTES"))
        );
    }

    #[tokio::test]
    async fn image_only_prompt_broadcast_renders_a_placeholder() {
        let transport = Arc::new(FakeTransport::default());
        let content = MessageContent::blocks(vec![ContentBlock::image("image/png", "SECRETBYTES")]);

        let prompt_transport: Arc<dyn ConversationTransport> = transport.clone();
        send_user_prompt(&prompt_transport, &content).await;

        assert_eq!(*transport.messages.lock().await, ["User: [图片]"]);
    }

    #[tokio::test]
    async fn final_stream_sends_only_the_last_assistant_message() {
        let transport = Arc::new(FakeTransport::default());
        let session_id = SessionId::parse("session-final-last").unwrap();
        let turn_id = TurnId::parse("turn-final-last").unwrap();
        let call = ActiveToolCall {
            tool_call_id: "call-1".to_string(),
            tool_name: "terminal".to_string(),
            raw_input: json!({"command":"echo hi"}),
            status: "in_progress".to_string(),
        };
        let (events_tx, events) = tokio::sync::mpsc::channel(16);
        let subscription = test_subscription(
            session_id.clone(),
            turn_id.clone(),
            SessionMode::FullAccess,
            events,
        );
        let task = tokio::spawn(stream_session(
            transport.clone(),
            EndpointId::new(),
            session_id.clone(),
            subscription,
        ));
        send_events(
            &events_tx,
            &session_id,
            [
                SessionEventPayload::AssistantCompleted {
                    message_id: MessageId::new(),
                    thought_message_id: MessageId::new(),
                    turn_id: turn_id.clone(),
                    content: "first answer".to_string(),
                    reasoning: Some("first thought".to_string()),
                    tool_calls: vec![call.clone()],
                },
                SessionEventPayload::ToolStarted {
                    turn_id: turn_id.clone(),
                    call,
                },
                SessionEventPayload::AssistantCompleted {
                    message_id: MessageId::new(),
                    thought_message_id: MessageId::new(),
                    turn_id: turn_id.clone(),
                    content: "final answer".to_string(),
                    reasoning: Some("final thought".to_string()),
                    tool_calls: Vec::new(),
                },
                SessionEventPayload::TurnCompleted { turn_id },
            ],
        )
        .await;
        drop(events_tx);
        task.await.unwrap();

        assert_eq!(*transport.messages.lock().await, ["final answer"]);
    }

    #[tokio::test]
    async fn permission_request_is_forwarded_in_final_mode() {
        let transport = Arc::new(FakeTransport::default());
        let session_id = SessionId::parse("session-permission").unwrap();
        let turn_id = TurnId::parse("turn-permission").unwrap();
        let call = ActiveToolCall {
            tool_call_id: "call-1".to_string(),
            tool_name: "terminal".to_string(),
            raw_input: json!({"command":"echo hi"}),
            status: "in_progress".to_string(),
        };
        let (events_tx, events) = tokio::sync::mpsc::channel(16);
        let subscription = test_subscription(
            session_id.clone(),
            turn_id.clone(),
            SessionMode::Confirm,
            events,
        );
        let task = tokio::spawn(stream_session(
            transport.clone(),
            EndpointId::new(),
            session_id.clone(),
            subscription,
        ));
        send_events(
            &events_tx,
            &session_id,
            [
                SessionEventPayload::ToolStarted {
                    turn_id: turn_id.clone(),
                    call,
                },
                SessionEventPayload::PermissionRequested {
                    turn_id: turn_id.clone(),
                    permission: PendingPermission {
                        request_id: "permission-1".to_string(),
                        tool_call_id: "call-1".to_string(),
                        tool_name: "terminal".to_string(),
                    },
                },
                SessionEventPayload::TurnCompleted { turn_id },
            ],
        )
        .await;
        drop(events_tx);
        task.await.unwrap();

        assert_eq!(*transport.messages.lock().await, ["CARD:call-1"]);
    }

    #[tokio::test]
    async fn cancelled_turn_flushes_the_last_message_with_a_marker() {
        let transport = Arc::new(FakeTransport::default());
        let session_id = SessionId::parse("session-cancel").unwrap();
        let turn_id = TurnId::parse("turn-cancel").unwrap();
        let (events_tx, events) = tokio::sync::mpsc::channel(16);
        let subscription = test_subscription(
            session_id.clone(),
            turn_id.clone(),
            SessionMode::FullAccess,
            events,
        );
        let task = tokio::spawn(stream_session(
            transport.clone(),
            EndpointId::new(),
            session_id.clone(),
            subscription,
        ));
        send_events(
            &events_tx,
            &session_id,
            [
                SessionEventPayload::AssistantCompleted {
                    message_id: MessageId::new(),
                    thought_message_id: MessageId::new(),
                    turn_id: turn_id.clone(),
                    content: "partial answer".to_string(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                },
                SessionEventPayload::TurnCancelled { turn_id },
            ],
        )
        .await;
        drop(events_tx);
        task.await.unwrap();

        assert_eq!(
            *transport.messages.lock().await,
            ["partial answer\n\nTurn cancelled"]
        );
    }

    fn test_subscription(
        session_id: SessionId,
        turn_id: TurnId,
        mode: SessionMode,
        events: tokio::sync::mpsc::Receiver<dwo_agent_service::SessionEvent>,
    ) -> SessionSubscription {
        SessionSubscription {
            snapshot: SessionSnapshot {
                record: SessionRecord::new(
                    session_id,
                    "Test".to_string(),
                    dwo_agent_service::SessionWorkspace::External {
                        pwd: PathBuf::from("."),
                    },
                    PathBuf::from("."),
                    mode,
                    SessionLlmSettings::default(),
                ),
                transcript: Vec::new(),
                checkpoint_cursor: 0,
                usage: SessionUsageSnapshot { used: 1, size: 2 },
                phase: RuntimePhase::Running,
                active_turn_id: Some(turn_id),
                active_step: None,
                partial_message: String::new(),
                active_tool_calls: Vec::new(),
                pending_permission: None,
                terminal_views: Vec::new(),
                live_terminal_ids: Vec::new(),
                seq: 0,
            },
            events,
        }
    }

    async fn send_events<const N: usize>(
        events: &tokio::sync::mpsc::Sender<dwo_agent_service::SessionEvent>,
        session_id: &SessionId,
        payloads: [SessionEventPayload; N],
    ) {
        for (seq, payload) in payloads.into_iter().enumerate() {
            events
                .send(dwo_agent_service::SessionEvent {
                    seq: seq as u64 + 1,
                    session_id: session_id.clone(),
                    payload,
                })
                .await
                .unwrap();
        }
    }
}
