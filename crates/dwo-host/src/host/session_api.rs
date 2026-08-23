use std::path::Path;

use anyhow::Result;
use dwo_agent_service::{
    ConfirmationDecision, EndpointId, MessageId, NotificationLevel, PromptAccepted,
    SessionConfigUpdate, SessionId, SessionRecord, SessionSnapshot, SessionStatusSnapshot,
    SessionSubscription, TurnId,
};
use dwo_context::MessageContent;
use serde_json::{Value, json};

use super::Host;
use dwo_command::{
    AvailableSkill, DirectiveKinds, directive_kinds, expand as expand_prompt_directives,
};

impl Host {
    pub async fn list_sessions(
        &self,
        all: bool,
        caller: Option<&SessionId>,
    ) -> Result<Vec<SessionRecord>> {
        let mut records = self.service.list().await?;
        if !all {
            records.retain(|record| record.info.parent_session_id.as_ref() == caller);
        }
        Ok(records)
    }

    pub async fn list_session_statuses(
        &self,
        all: bool,
        caller: Option<&SessionId>,
    ) -> Result<Vec<SessionStatusSnapshot>> {
        let mut statuses = self.service.list_statuses().await?;
        if !all {
            statuses.retain(|status| status.record.info.parent_session_id.as_ref() == caller);
        }
        Ok(statuses)
    }

    pub async fn session_status(&self, id: &SessionId) -> Result<SessionStatusSnapshot> {
        Ok(self.service.status(id).await?)
    }

    pub async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot> {
        Ok(self.service.load(id).await?.snapshot().await?)
    }

    pub async fn setup_session(
        &self,
        title: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<SessionSnapshot> {
        Ok(self.create_session(title, cwd).await?.snapshot().await?)
    }

    pub async fn setup_project_session(
        &self,
        title: Option<String>,
        project_id: &str,
        topic_id: Option<&str>,
    ) -> Result<SessionSnapshot> {
        let project = self.projects.get(project_id)?;
        let topic_id = topic_id.unwrap_or(&project.board.uncategorized_topic_id);
        Ok(self
            .create_project_session(title, project_id, topic_id)
            .await?
            .snapshot()
            .await?)
    }

    pub async fn fork_session(&self, source_id: &SessionId) -> Result<SessionSnapshot> {
        let snapshot = self.service.fork(source_id, None).await?.snapshot().await?;
        if let Some((project, topic)) = self.projects.locate_session(source_id.as_str()) {
            self.projects.assign_session(
                &project.id,
                &topic.id,
                snapshot.record.info.id.to_string(),
            )?;
        }
        Ok(snapshot)
    }

    pub async fn subscribe_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription> {
        Ok(self
            .service
            .load(id)
            .await?
            .attach_from(endpoint, checkpoint_cursor)
            .await?)
    }

    pub async fn prompt_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted> {
        let agent = self.service.load(id).await?;
        let snapshot = agent.snapshot().await?;
        let content = self
            .expand_prompt_directives(&snapshot.record.info.cwd, content)
            .await?;
        Ok(self.service.prompt(id, endpoint, content).await?)
    }

    pub async fn publish_session_notification(
        &self,
        id: &SessionId,
        origin: Option<EndpointId>,
        category: String,
        level: NotificationLevel,
        text: String,
        data: Value,
    ) -> Result<MessageId> {
        Ok(self
            .service
            .load(id)
            .await?
            .publish_notification(origin, category, level, text, data)
            .await?)
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
            self.service
                .skill_snapshots(cwd)?
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
        let snapshot = self.session_snapshot(id).await?;
        let skills = self
            .service
            .skill_snapshots(&snapshot.record.info.cwd)?
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

    pub async fn compact_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<PromptAccepted> {
        Ok(self.service.load(id).await?.compact(endpoint).await?)
    }

    pub async fn resume_session_turn(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<Option<PromptAccepted>> {
        Ok(self.service.load(id).await?.resume(endpoint).await?)
    }

    pub async fn cancel_session(
        &self,
        id: &SessionId,
        expected_turn_id: Option<TurnId>,
    ) -> Result<()> {
        self.service
            .load(id)
            .await?
            .cancel(expected_turn_id)
            .await?;
        Ok(())
    }

    pub async fn close_session(&self, id: &SessionId) -> Result<()> {
        self.service.close(id).await?;
        Ok(())
    }

    pub async fn set_session_config(
        &self,
        id: &SessionId,
        update: SessionConfigUpdate,
    ) -> Result<SessionSnapshot> {
        let agent = self.service.load(id).await?;
        agent.set_config(update).await?;
        Ok(agent.snapshot().await?)
    }

    pub async fn resolve_session_permission(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<()> {
        self.service
            .load(id)
            .await?
            .respond_permission(endpoint, request_id, decision)
            .await?;
        Ok(())
    }
}
