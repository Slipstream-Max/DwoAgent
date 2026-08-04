use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use dwo_agent_service::{
    ActiveToolCall, EndpointId, MessageContent, PendingPermission, SessionConfigUpdate,
    SessionEventPayload, SessionId,
};
use dwo_tools::{ConfirmationDecision, SessionMode};
use tokio::sync::Mutex;

use crate::host::Host;

use super::command::{ChannelCommand, render_command_help};
use super::render::{
    SessionStreamState, display_path, policy_name, render_live_user_prompt, render_session_replay,
    render_status, render_tool_call,
};

#[async_trait]
pub(crate) trait ConversationTransport: Send + Sync {
    async fn send_text(&self, text: &str) -> Result<()>;
    async fn save_selected_session(&self, session_id: Option<&str>) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConversationId {
    channel: String,
    peer_id: String,
}

impl ConversationId {
    pub(crate) fn new(channel: impl Into<String>, peer_id: impl Into<String>) -> Self {
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

pub(crate) struct SessionBridge {
    host: Arc<Host>,
    conversation: ConversationId,
    endpoint: EndpointId,
    replay_turns: usize,
    selected_session_id: Mutex<Option<String>>,
    transport: Arc<dyn ConversationTransport>,
    observer: Mutex<Option<SessionObserver>>,
}

impl SessionBridge {
    pub(crate) fn new(
        host: Arc<Host>,
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
            transport,
            observer: Mutex::new(None),
        }
    }

    pub(crate) async fn execute(&self, command: ChannelCommand) -> Result<Vec<String>> {
        let messages = match command {
            ChannelCommand::Help => vec![render_command_help()],
            ChannelCommand::List => {
                let records = self.host.list_sessions(true, None).await?;
                let selected = self.selected_session_id.lock().await.clone();
                let text = records
                    .into_iter()
                    .map(|record| {
                        format!(
                            "{} {} {}",
                            if selected.as_deref() == Some(record.info.id.as_str()) {
                                "*"
                            } else {
                                "-"
                            },
                            record.info.id,
                            record.info.title
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                vec![if text.is_empty() {
                    "No sessions".to_string()
                } else {
                    text
                }]
            }
            ChannelCommand::New { name, cwd } => {
                let title = (!name.is_empty()).then(|| name.join(" "));
                let snapshot = self.host.setup_session(title, cwd).await?;
                let session_id = snapshot.record.info.id.clone();
                self.select_session(session_id.as_str()).await?;
                vec![format!(
                    "Selected new session {}\nCwd: {}",
                    session_id,
                    display_path(&snapshot.record.info.cwd)
                )]
            }
            ChannelCommand::Use { session: id } => {
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
            ChannelCommand::Del { session: id } => {
                let session_id = SessionId::parse(id.clone()).map_err(anyhow::Error::msg)?;
                self.host.delete_session(&session_id).await?;
                if self.selected_session_id.lock().await.as_deref() == Some(id.as_str()) {
                    self.set_selected_session(None).await?;
                }
                vec!["Session deleted".to_string()]
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
                    .resume_session_turn(&id, self.endpoint.clone())
                    .await?;
                Vec::new()
            }
            ChannelCommand::Model { name: model } => {
                let id = self.selected_session_id().await?;
                self.host
                    .set_session_config(&id, SessionConfigUpdate::Model(model))
                    .await?;
                vec!["Model updated".to_string()]
            }
            ChannelCommand::Reasoning { level } => {
                let reasoning = (level != "off").then_some(level);
                let id = self.selected_session_id().await?;
                self.host
                    .set_session_config(&id, SessionConfigUpdate::Reasoning(reasoning))
                    .await?;
                vec!["Reasoning updated".to_string()]
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

    pub(crate) async fn submit_prompt(&self, content: MessageContent) -> Result<()> {
        let session_id = self.ensure_prompt_session().await?;
        self.ensure_observer(&session_id).await?;
        self.host
            .prompt_session(&session_id, self.endpoint.clone(), content)
            .await?;
        Ok(())
    }

    pub(crate) async fn ensure_prompt_session(&self) -> Result<SessionId> {
        if let Some(id) = self.selected_session_id.lock().await.clone() {
            return SessionId::parse(id).map_err(anyhow::Error::msg);
        }
        let session_id = {
            let snapshot = self.host.setup_session(None, None).await?;
            let session_id = snapshot.record.info.id;
            self.select_session(session_id.as_str()).await?;
            session_id
        };
        Ok(session_id)
    }

    pub(crate) async fn resume_observer(&self) -> Result<()> {
        let Some(id) = self.selected_session_id.lock().await.clone() else {
            return Ok(());
        };
        let session_id = SessionId::parse(id).map_err(anyhow::Error::msg)?;
        self.ensure_observer(&session_id).await
    }

    pub(crate) async fn stop(&self) {
        if let Some(observer) = self.observer.lock().await.take() {
            observer.task.abort();
        }
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
            subscription,
        ));
        *observer = Some(SessionObserver { session_id, task });
        Ok(())
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
                if let Some(prompt) = render_live_user_prompt(&content) {
                    send(&transport, &prompt).await;
                }
            }
            SessionEventPayload::AssistantCompleted {
                content,
                tool_calls,
                ..
            } => {
                stream.remember_response(content);
                for call in tool_calls {
                    stream.remember_tool(call);
                }
            }
            SessionEventPayload::ToolStarted { call, .. } => stream.remember_tool(call),
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
                    });
                send(
                    &transport,
                    &render_tool_call(&call, "request id", &permission.request_id),
                )
                .await;
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
                stream.remember_response("Turn cancelled".to_string());
                if let Some(response) = stream.take_response() {
                    send(&transport, &response).await;
                }
                stream.finish_turn();
            }
            SessionEventPayload::TurnFailed { error, .. } => {
                stream.remember_response(format!("Turn failed: {error}"));
                if let Some(response) = stream.take_response() {
                    send(&transport, &response).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use dwo_agent_service::{
        ClientTranscriptEvent, MessageId, RuntimePhase, SessionLlmSettings, SessionRecord,
        SessionSnapshot, SessionSubscription, SessionUsageSnapshot, TurnId,
    };
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
            PathBuf::from("."),
            SessionMode::Confirm,
            SessionLlmSettings::default(),
            dwo_agent_service::DEFAULT_MAX_MODEL_STEPS,
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
                seq: 0,
            },
            events,
        };
        let task = tokio::spawn(stream_session(
            transport.clone(),
            EndpointId::new(),
            subscription,
        ));
        for (seq, payload) in [
            SessionEventPayload::UserPromptSubmitted {
                message_id: MessageId::new(),
                turn_id: turn_id.clone(),
                origin: EndpointId::new(),
                content: MessageContent::text("question"),
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
}
