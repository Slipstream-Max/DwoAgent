use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use dwo_agent_service::{
    ActiveToolCall, EndpointId, MessageContent, PendingPermission, SessionConfigUpdate,
    SessionEventPayload, SessionId,
};
use dwo_tools::{ConfirmationDecision, SessionMode};
use tokio::sync::Mutex;

use super::ChannelHost;
use dwo_command::{ChannelCommand, render_command_help};

use super::manager::ChannelOutputMode;
use super::render::{
    OutputSegment, SessionStreamState, display_path, policy_name, render_live_user_prompt,
    render_output_segment, render_session_replay, render_status, render_tool_call,
    short_session_id, short_session_id_str,
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
    fn max_text_chars(&self) -> usize {
        4_000
    }
    async fn send_segment(&self, segment: &OutputSegment) -> Result<()> {
        self.send_text(&render_output_segment(segment)).await
    }
    fn defer_tool_call_to_permission(&self, _mode: SessionMode) -> bool {
        false
    }
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
    output_mode: ChannelOutputMode,
    selected_session_id: Mutex<Option<String>>,
    session_choices: Mutex<Vec<String>>,
    transport: Arc<dyn ConversationTransport>,
    observer: Mutex<Option<SessionObserver>>,
}

impl SessionBridge {
    pub fn new(
        host: Arc<dyn ChannelHost>,
        conversation: ConversationId,
        replay_turns: usize,
        output_mode: ChannelOutputMode,
        selected_session_id: Option<String>,
        transport: Arc<dyn ConversationTransport>,
    ) -> Self {
        let endpoint = conversation.endpoint();
        Self {
            host,
            conversation,
            endpoint,
            replay_turns,
            output_mode,
            selected_session_id: Mutex::new(selected_session_id),
            session_choices: Mutex::new(Vec::new()),
            transport,
            observer: Mutex::new(None),
        }
    }

    pub async fn execute(&self, command: ChannelCommand) -> Result<Vec<String>> {
        let messages = match command {
            ChannelCommand::Help => vec![render_command_help()],
            ChannelCommand::List => {
                let records = self.host.list_sessions(true, None).await?;
                let selected = self.selected_session_id.lock().await.clone();
                let choices = records
                    .iter()
                    .map(|record| record.info.id.to_string())
                    .collect::<Vec<_>>();
                *self.session_choices.lock().await = choices;
                let text = records
                    .iter()
                    .enumerate()
                    .map(|(index, record)| {
                        format!(
                            "{} {}. {} [{}]",
                            if selected.as_deref() == Some(record.info.id.as_str()) {
                                "*"
                            } else {
                                " "
                            },
                            index + 1,
                            short_session_title(&record.info.title),
                            short_session_id(&record.info.id),
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
            ChannelCommand::Fork => {
                let source_id = self.selected_session_id().await?;
                let snapshot = self.host.fork_session(&source_id).await?;
                vec![format!(
                    "Forked session {}\nTitle: {}",
                    snapshot.record.info.id, snapshot.record.info.title
                )]
            }
            ChannelCommand::Use { session: reference } => {
                let id = self.resolve_session_reference(&reference).await?;
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
            ChannelCommand::Del { session: reference } => {
                let id = self.resolve_session_reference(&reference).await?;
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
            let snapshot = self.host.setup_session(None, None).await?;
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
            let choices = self.session_choices.lock().await;
            let Some(id) = index.checked_sub(1).and_then(|index| choices.get(index)) else {
                anyhow::bail!(
                    "Session number {index} is not in the last /list result; send /list first"
                );
            };
            return Ok(id.clone());
        }
        if let Ok(id) = SessionId::parse(reference.to_string()) {
            return Ok(id.to_string());
        }

        let records = self.host.list_sessions(true, None).await?;
        let matches = records
            .into_iter()
            .filter(|record| session_id_matches(&record.info.id, reference))
            .map(|record| record.info.id.to_string())
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
        let output_mode = self.output_mode;
        let task = tokio::spawn(stream_session_with_mode(
            transport,
            self.endpoint.clone(),
            id.clone(),
            output_mode,
            subscription,
        ));
        *observer = Some(SessionObserver { session_id, task });
        Ok(())
    }
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
    fn short_ids_are_copyable_without_the_session_prefix() {
        let id = SessionId::parse("session-1234567890abcdef").unwrap();
        assert_eq!(short_session_id(&id), "12345678");
        assert!(session_id_matches(&id, "12345678"));
        assert!(session_id_matches(&id, "session-123456"));
        assert!(!session_id_matches(&id, "abcdef"));
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

#[cfg(test)]
async fn stream_session(
    transport: Arc<dyn ConversationTransport>,
    endpoint: EndpointId,
    session_id: SessionId,
    subscription: dwo_agent_service::SessionSubscription,
) {
    stream_session_with_mode(
        transport,
        endpoint,
        session_id,
        ChannelOutputMode::Final,
        subscription,
    )
    .await;
}

async fn stream_session_with_mode(
    transport: Arc<dyn ConversationTransport>,
    endpoint: EndpointId,
    session_id: SessionId,
    output_mode: ChannelOutputMode,
    mut subscription: dwo_agent_service::SessionSubscription,
) {
    let mut stream = SessionStreamState::default();
    let mut session_mode = subscription.snapshot.record.info.mode;
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
                reasoning,
                content,
                tool_calls,
                ..
            } => {
                if output_mode == ChannelOutputMode::Full {
                    stream.remember_reasoning(reasoning);
                    for reasoning in stream.take_reasoning(transport.max_text_chars()) {
                        send_segment(&transport, &reasoning).await;
                    }
                    for call in &tool_calls {
                        stream.remember_tool(call.clone());
                    }
                    stream.remember_full_response(content, &tool_calls);
                    send_ready_full_responses(&transport, &mut stream).await;
                } else {
                    stream.remember_response(content);
                    for call in tool_calls {
                        stream.remember_tool(call);
                    }
                }
            }
            SessionEventPayload::ToolStarted { call, .. } => {
                let defer_to_permission = transport.defer_tool_call_to_permission(session_mode);
                if output_mode == ChannelOutputMode::Full && !defer_to_permission {
                    send_segment(&transport, &OutputSegment::ToolCall(call.clone())).await;
                    stream.mark_tool_presented(&call.tool_call_id);
                }
                stream.remember_tool(call);
                send_ready_full_responses(&transport, &mut stream).await;
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
                } else if output_mode == ChannelOutputMode::Full {
                    stream.mark_tool_presented(&permission.tool_call_id);
                    send_ready_full_responses(&transport, &mut stream).await;
                }
            }
            SessionEventPayload::ToolCompleted { result, .. } => {
                if output_mode == ChannelOutputMode::Full
                    && !stream.tool_was_presented(&result.tool_call_id)
                    && let Some(call) = stream.tool(&result.tool_call_id).cloned()
                {
                    send_segment(&transport, &OutputSegment::ToolCall(call)).await;
                    stream.mark_tool_presented(&result.tool_call_id);
                    send_ready_full_responses(&transport, &mut stream).await;
                }
                stream.forget_tool(&result.tool_call_id);
            }
            SessionEventPayload::TurnCompleted { .. } => {
                if output_mode == ChannelOutputMode::Full {
                    for response in stream.take_all_full_responses() {
                        send_segment(&transport, &response).await;
                    }
                } else if let Some(response) = stream.take_response() {
                    send(&transport, &response).await;
                }
                stream.finish_turn();
            }
            SessionEventPayload::TurnCancelled { .. } => {
                if output_mode == ChannelOutputMode::Full {
                    for response in stream.take_all_full_responses() {
                        send_segment(&transport, &response).await;
                    }
                    send(&transport, "Turn cancelled").await;
                } else {
                    stream.remember_response("Turn cancelled".to_string());
                    if let Some(response) = stream.take_response() {
                        send(&transport, &response).await;
                    }
                }
                stream.finish_turn();
            }
            SessionEventPayload::TurnFailed { error, .. } => {
                if output_mode == ChannelOutputMode::Full {
                    for response in stream.take_all_full_responses() {
                        send_segment(&transport, &response).await;
                    }
                    send(&transport, &format!("Turn failed: {error}")).await;
                } else {
                    stream.remember_response(format!("Turn failed: {error}"));
                    if let Some(response) = stream.take_response() {
                        send(&transport, &response).await;
                    }
                }
                stream.finish_turn();
            }
            SessionEventPayload::ConfigChanged { config } => {
                session_mode = config.mode;
            }
            _ => {}
        }
    }
}

async fn send_ready_full_responses(
    transport: &Arc<dyn ConversationTransport>,
    stream: &mut SessionStreamState,
) {
    for response in stream.take_ready_full_responses() {
        send_segment(transport, &response).await;
    }
}

async fn send_segment(transport: &Arc<dyn ConversationTransport>, segment: &OutputSegment) {
    if let Err(error) = transport.send_segment(segment).await {
        tracing::warn!(
            event = "channel.message_send_failed",
            error = %format!("{error:#}"),
            "send channel message failed"
        );
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
    use serde_json::json;
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeTransport {
        messages: Mutex<Vec<String>>,
        selected: Mutex<Option<String>>,
        defer_to_permission: bool,
    }

    #[async_trait]
    impl ConversationTransport for FakeTransport {
        async fn send_text(&self, text: &str) -> Result<()> {
            self.messages.lock().await.push(text.to_string());
            Ok(())
        }

        fn defer_tool_call_to_permission(&self, mode: SessionMode) -> bool {
            self.defer_to_permission && mode == SessionMode::Confirm
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
    async fn full_replay_preserves_thinking_tool_answer_step_order() {
        let transport = Arc::new(FakeTransport::default());
        let session_id = SessionId::parse("session-full-order").unwrap();
        let turn_id = TurnId::parse("turn-full-order").unwrap();
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
        let task = tokio::spawn(stream_session_with_mode(
            transport.clone(),
            EndpointId::new(),
            session_id.clone(),
            ChannelOutputMode::Full,
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
                    call: call.clone(),
                },
                SessionEventPayload::ToolCompleted {
                    turn_id: turn_id.clone(),
                    result: dwo_tools::ToolResult {
                        tool_call_id: call.tool_call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        output: json!({"status":"completed"}),
                        model_context: Vec::new(),
                    },
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

        assert_eq!(
            *transport.messages.lock().await,
            [
                "🧠 Thinking:\nfirst thought",
                "🔧Tool Call:\n```\necho hi\n```\ntool call id：call-1",
                "first answer",
                "🧠 Thinking:\nfinal thought",
                "final answer",
            ]
        );
    }

    #[tokio::test]
    async fn confirm_card_replaces_the_duplicate_full_replay_tool_message() {
        let transport = Arc::new(FakeTransport {
            defer_to_permission: true,
            ..Default::default()
        });
        let session_id = SessionId::parse("session-confirm-card").unwrap();
        let turn_id = TurnId::parse("turn-confirm-card").unwrap();
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
        let task = tokio::spawn(stream_session_with_mode(
            transport.clone(),
            EndpointId::new(),
            session_id.clone(),
            ChannelOutputMode::Full,
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
                    content: "answer after permission".to_string(),
                    reasoning: Some("inspect first".to_string()),
                    tool_calls: vec![call.clone()],
                },
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

        assert_eq!(
            *transport.messages.lock().await,
            [
                "🧠 Thinking:\ninspect first",
                "CARD:call-1",
                "answer after permission",
            ]
        );
    }

    #[tokio::test]
    async fn confirm_card_transport_still_shows_tools_that_need_no_permission() {
        let transport = Arc::new(FakeTransport {
            defer_to_permission: true,
            ..Default::default()
        });
        let session_id = SessionId::parse("session-confirm-hosted").unwrap();
        let turn_id = TurnId::parse("turn-confirm-hosted").unwrap();
        let call = ActiveToolCall {
            tool_call_id: "search-1".to_string(),
            tool_name: "web_search".to_string(),
            raw_input: json!({"query":"latest release"}),
            status: "in_progress".to_string(),
        };
        let (events_tx, events) = tokio::sync::mpsc::channel(16);
        let subscription = test_subscription(
            session_id.clone(),
            turn_id.clone(),
            SessionMode::Confirm,
            events,
        );
        let task = tokio::spawn(stream_session_with_mode(
            transport.clone(),
            EndpointId::new(),
            session_id.clone(),
            ChannelOutputMode::Full,
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
                    content: "search answer".to_string(),
                    reasoning: Some("search first".to_string()),
                    tool_calls: vec![call.clone()],
                },
                SessionEventPayload::ToolStarted {
                    turn_id: turn_id.clone(),
                    call: call.clone(),
                },
                SessionEventPayload::ToolCompleted {
                    turn_id: turn_id.clone(),
                    result: dwo_tools::ToolResult {
                        tool_call_id: call.tool_call_id,
                        tool_name: call.tool_name,
                        output: json!({"status":"completed"}),
                        model_context: Vec::new(),
                    },
                },
                SessionEventPayload::TurnCompleted { turn_id },
            ],
        )
        .await;
        drop(events_tx);
        task.await.unwrap();

        assert_eq!(
            *transport.messages.lock().await,
            [
                "🧠 Thinking:\nsearch first",
                "🔧Tool Call:\n```\n{\n  \"query\": \"latest release\"\n}\n```\ntool call id：search-1",
                "search answer",
            ]
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
