use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use dwo_agent_service::{
    CompactionAccepted, ConfirmationDecision, EndpointId, PromptAccepted, SessionConfigUpdate,
    RuntimePhase, SessionId, SessionListItem, SessionListQuery, SessionSnapshot,
    SessionSubscription, TurnId,
};
use dwo_channels::{ChannelHost, ChannelManager, ChannelSessionItem, SessionCreateRequest};
use dwo_context::MessageContent;

use super::{Host, HostSessionOptions};

#[async_trait::async_trait]
impl ChannelHost for Host {
    fn profile_root_path(&self) -> &Path {
        &self.profile_root
    }

    fn channels(&self) -> Arc<ChannelManager> {
        self.channels
            .read()
            .expect("channel manager lock poisoned")
            .clone()
    }

    async fn list_sessions(
        &self,
        all: bool,
        caller: Option<&SessionId>,
    ) -> Result<Vec<SessionListItem>> {
        let mut query = SessionListQuery::new(None, Some(500));
        if !all {
            query.parent_session_id = caller.cloned();
            query.roots_only = caller.is_none();
        }
        let mut sessions = Vec::new();
        loop {
            let page = self.service.list(query.clone()).await?;
            sessions.extend(page.sessions);
            if let Some(next) = page.next_cursor {
                query.cursor = Some(next);
            } else {
                return Ok(sessions);
            }
        }
    }

    async fn session_options(&self, id: &SessionId) -> Result<dwo_protocol::SessionOptions> {
        let record = self.service.snapshot(id).await?.record;
        let models = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .model_options
            .clone();
        Ok(serde_json::from_value(serde_json::to_value(
            super::SessionOptionSnapshot {
                config: record.config(),
                models,
            },
        )?)?)
    }

    async fn list_channel_sessions(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<ChannelSessionItem>> {
        let project = project
            .map(|name| resolve_project(self, name))
            .transpose()?;
        let sessions = <Self as ChannelHost>::list_sessions(self, false, None).await?;
        let mut rows = Vec::new();
        for session in sessions {
            let section = if let Some(project) = &project {
                let Some(assignment) = project
                    .session_assignments
                    .iter()
                    .find(|a| a.session_id == session.session_id.as_str())
                else {
                    continue;
                };
                let section = project
                    .sections
                    .iter()
                    .find(|s| s.id == assignment.section_id);
                section.map(|s| (s.order, s.name.clone()))
            } else {
                None
            };
            let working = !matches!(
                session.status,
                RuntimePhase::Idle | RuntimePhase::Closing
            );
            rows.push((section, working, session));
        }
        rows.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| b.2.updated_at.cmp(&a.2.updated_at))
                .then_with(|| a.2.session_id.as_str().cmp(b.2.session_id.as_str()))
        });
        Ok(rows
            .into_iter()
            .map(|(section, working, session)| ChannelSessionItem {
                session,
                section: section.map(|(_, name)| name),
                working,
            })
            .collect())
    }

    async fn list_skills(&self, session: Option<&SessionId>) -> Result<Vec<(String, String)>> {
        let cwd = match session {
            Some(id) => self.service.snapshot(id).await?.record.info.cwd,
            None => self.profile_root.clone(),
        };
        let external = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .config
            .external_skills_dirs
            .clone();
        let skills = super::skill_api::skill_snapshots(&self.profile_root, &external, &cwd)?
            .into_iter()
            .filter(|skill| !skill.name.chars().any(char::is_whitespace))
            .map(|skill| (skill.name, skill.description))
            .collect();
        Ok(skills)
    }

    async fn create_session(&self, request: SessionCreateRequest) -> Result<SessionSnapshot> {
        let project = request
            .project
            .as_deref()
            .map(|name| resolve_project(self, name))
            .transpose()?;
        let section_id = match (&project, request.section.as_deref()) {
            (Some(project), Some(name)) => {
                let matches: Vec<_> = project.sections.iter().filter(|s| s.name == name).collect();
                anyhow::ensure!(
                    matches.len() == 1,
                    "Section '{name}' is missing or ambiguous"
                );
                Some(matches[0].id.clone())
            }
            (None, Some(_)) => anyhow::bail!("A section requires a project"),
            _ => None,
        };
        let id = self
            .create_session(HostSessionOptions {
                project_id: project.map(|p| p.id),
                section_id,
                title: request.title,
                cwd: request.cwd,
                from: request.from,
                ..HostSessionOptions::default()
            })
            .await?;
        Ok(self.service.snapshot(&id).await?)
    }

    async fn subscribe_session(
        &self,
        id: &SessionId,
        _endpoint: EndpointId,
        checkpoint_cursor: Option<usize>,
    ) -> Result<SessionSubscription> {
        Ok(self.service.subscribe(id, checkpoint_cursor).await?)
    }

    async fn session_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot> {
        Ok(self.service.snapshot(id).await?)
    }

    async fn archive_session(&self, id: &SessionId) -> Result<()> {
        Host::archive_session(self, id).await
    }

    async fn cancel_session(&self, id: &SessionId, expected_turn_id: Option<TurnId>) -> Result<()> {
        Ok(self.service.cancel(id, expected_turn_id).await?)
    }

    async fn compact_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
    ) -> Result<CompactionAccepted> {
        Ok(self.service.compact(id, endpoint).await?)
    }

    async fn prompt_internal(&self, id: &SessionId, content: MessageContent) -> Result<TurnId> {
        Ok(self.service.prompt_internal(id, content).await?)
    }

    async fn set_session_config(&self, id: &SessionId, update: SessionConfigUpdate) -> Result<()> {
        Ok(self.service.set_config(id, update).await?)
    }

    async fn resolve_session_permission(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        request_id: String,
        decision: ConfirmationDecision,
    ) -> Result<()> {
        Ok(self
            .service
            .respond_permission(id, endpoint, request_id, decision)
            .await?)
    }

    async fn prompt_session(
        &self,
        id: &SessionId,
        endpoint: EndpointId,
        content: MessageContent,
    ) -> Result<PromptAccepted> {
        Host::prompt_session(self, id, endpoint, content).await
    }
}

fn resolve_project(host: &Host, name: &str) -> Result<dwo_project::Project> {
    let matches: Vec<_> = host
        .projects
        .list()
        .into_iter()
        .filter(|p| p.name == name)
        .collect();
    anyhow::ensure!(
        matches.len() == 1,
        "Project '{name}' is missing or ambiguous"
    );
    Ok(matches.into_iter().next().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn channel_creation_scopes_and_archive_preserve_sessions() {
        let root = tempfile::tempdir().unwrap();
        let host = Host::build(super::super::tests::write_test_profile(root.path()))
            .await
            .unwrap();
        let project = host
            .projects
            .create(dwo_project::CreateProject {
                name: Some("My project".into()),
                pwd: root.path().to_path_buf(),
            })
            .unwrap();
        let section = host
            .projects
            .create_section(&project.id, "Development".into(), Some("#ffffff".into()))
            .unwrap();
        let plain = <Host as ChannelHost>::create_session(&host, SessionCreateRequest::default())
            .await
            .unwrap();
        assert!(matches!(
            plain.record.info.workspace,
            dwo_agent_service::SessionWorkspace::Managed
        ));
        assert!(
            host.projects
                .locate_session(plain.record.info.id.as_str())
                .is_none()
        );
        let default = <Host as ChannelHost>::create_session(
            &host,
            SessionCreateRequest {
                project: Some("My project".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            host.projects
                .locate_session(default.record.info.id.as_str())
                .unwrap()
                .1
                .section_id,
            project.default_section_id
        );
        let scoped = <Host as ChannelHost>::create_session(
            &host,
            SessionCreateRequest {
                project: Some("My project".into()),
                section: Some("Development".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            host.projects
                .locate_session(scoped.record.info.id.as_str())
                .unwrap()
                .1
                .section_id,
            section.id
        );
        assert_eq!(
            host.list_channel_sessions(Some("My project"))
                .await
                .unwrap()
                .len(),
            2
        );
        let inbox = host.list_channel_sessions(None).await.unwrap();
        assert_eq!(inbox.len(), 3);
        assert!(inbox.iter().all(|item| !item.working));
        assert!(host.list_skills(None).await.is_ok());
        assert!(
            <Host as ChannelHost>::create_session(
                &host,
                SessionCreateRequest {
                    project: Some("Missing".into()),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        assert!(
            <Host as ChannelHost>::create_session(
                &host,
                SessionCreateRequest {
                    project: Some("My project".into()),
                    section: Some("Missing".into()),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        <Host as ChannelHost>::archive_session(&host, &scoped.record.info.id)
            .await
            .unwrap();
        assert!(
            host.service
                .is_archived(&scoped.record.info.id)
                .await
                .unwrap()
        );
        assert_eq!(
            host.list_channel_sessions(Some("My project"))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(host.list_channel_sessions(None).await.unwrap().len(), 2);
        host.shutdown().await;
    }
}
