//! Channel command control, session switching, and single-holder leases.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use crate::agent::constants::{PERMISSION_ALLOW_ONCE, PERMISSION_REJECT_ONCE};
use crate::agent::service::AgentService;
use crate::agent::session_agent::SessionAgent;
use crate::config::loader::{channel_secret_dir, utc_iso};
use crate::config::models::{AgentState, ReasoningMode, SessionMetaPayload};
use crate::protocol::dwo::{self, DwoChannelCommand};
use crate::tools::{PermissionRequester, UpdateEmitter};
use crate::utils::files::read_utf8_text;

const CHANNEL_CONTROL_STATE_FILE: &str = "bridge_state.yaml";
const CONFIRM_AUDIT_FILE: &str = "confirm_audit.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelControlState {
    #[serde(
        default,
        alias = "default_session_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub home_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionLease {
    pub session_id: String,
    pub holder: String,
    pub acquired_at: String,
    pub updated_at: String,
}

#[derive(Default)]
pub struct SessionLeaseRegistry {
    leases: Mutex<HashMap<String, SessionLease>>,
}

impl SessionLeaseRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn acquire(&self, session_id: &str, holder: &str) -> Result<SessionLease> {
        self.acquire_sync(session_id, holder)
    }

    pub fn acquire_sync(&self, session_id: &str, holder: &str) -> Result<SessionLease> {
        let now = utc_iso();
        let mut leases = self.leases.lock().expect("session lease mutex poisoned");
        if let Some(existing) = leases.get_mut(session_id) {
            if existing.holder != holder {
                bail!(
                    "session {session_id} is occupied by {}; switch denied.",
                    existing.holder
                );
            }
            existing.updated_at = now.clone();
            return Ok(existing.clone());
        }

        let lease = SessionLease {
            session_id: session_id.to_string(),
            holder: holder.to_string(),
            acquired_at: now.clone(),
            updated_at: now,
        };
        leases.insert(session_id.to_string(), lease.clone());
        Ok(lease)
    }

    pub async fn release_if_holder(&self, session_id: &str, holder: &str) -> bool {
        self.release_if_holder_sync(session_id, holder)
    }

    pub fn release_if_holder_sync(&self, session_id: &str, holder: &str) -> bool {
        let mut leases = self.leases.lock().expect("session lease mutex poisoned");
        if leases
            .get(session_id)
            .is_some_and(|lease| lease.holder == holder)
        {
            leases.remove(session_id);
            true
        } else {
            false
        }
    }

    pub async fn holder(&self, session_id: &str) -> Option<String> {
        self.holder_sync(session_id)
    }

    pub fn holder_sync(&self, session_id: &str) -> Option<String> {
        self.leases
            .lock()
            .expect("session lease mutex poisoned")
            .get(session_id)
            .map(|lease| lease.holder.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingConfirmationSnapshot {
    pub confirmation_id: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_input: Value,
    pub requested_by: String,
    pub created_at: String,
}

struct PendingConfirmation {
    snapshot: PendingConfirmationSnapshot,
    responder: oneshot::Sender<String>,
}

pub struct PendingConfirmationRegistry {
    pending: Mutex<HashMap<String, PendingConfirmation>>,
    audit_path: PathBuf,
}

impl PendingConfirmationRegistry {
    pub fn new(agent_structure_dir: &Path) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            audit_path: channel_secret_dir(agent_structure_dir)
                .join("audit")
                .join(CONFIRM_AUDIT_FILE),
        }
    }

    pub fn create(
        &self,
        session_id: impl Into<String>,
        payload: Map<String, Value>,
        requested_by: impl Into<String>,
    ) -> Result<(PendingConfirmationSnapshot, oneshot::Receiver<String>)> {
        let tool_call_id = payload
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let tool_name = payload
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();
        let raw_input = payload.get("raw_input").cloned().unwrap_or(Value::Null);
        let confirmation_id = format!("c_{}", uuid::Uuid::new_v4().simple());
        let snapshot = PendingConfirmationSnapshot {
            confirmation_id: confirmation_id.clone(),
            session_id: session_id.into(),
            tool_call_id,
            tool_name,
            raw_input,
            requested_by: requested_by.into(),
            created_at: utc_iso(),
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending confirmation mutex poisoned")
            .insert(
                confirmation_id,
                PendingConfirmation {
                    snapshot: snapshot.clone(),
                    responder: tx,
                },
            );
        self.audit("requested", &snapshot, None, None)?;
        Ok((snapshot, rx))
    }

    pub fn resolve(
        &self,
        confirmation_id: &str,
        actor: &str,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<PendingConfirmationSnapshot> {
        let pending = self
            .pending
            .lock()
            .expect("pending confirmation mutex poisoned")
            .remove(confirmation_id)
            .ok_or_else(|| anyhow::anyhow!("找不到待确认请求：{confirmation_id}"))?;
        let snapshot = pending.snapshot;
        let _ = pending
            .responder
            .send(permission_decision_with_reason(decision, reason));
        self.audit(decision, &snapshot, Some(actor), reason)?;
        Ok(snapshot)
    }

    pub fn snapshot(&self, confirmation_id: &str) -> Option<PendingConfirmationSnapshot> {
        self.pending
            .lock()
            .expect("pending confirmation mutex poisoned")
            .get(confirmation_id)
            .map(|pending| pending.snapshot.clone())
    }

    fn audit(
        &self,
        event: &str,
        snapshot: &PendingConfirmationSnapshot,
        actor: Option<&str>,
        reason: Option<&str>,
    ) -> Result<()> {
        if let Some(parent) = self.audit_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_path)
            .with_context(|| format!("open {}", self.audit_path.display()))?;
        let line = serde_json::to_string(&json!({
            "updated_at": utc_iso(),
            "event": event,
            "actor": actor,
            "reason": reason,
            "confirmation": snapshot,
        }))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct ChannelControl {
    agent: Arc<AgentService>,
    leases: Arc<SessionLeaseRegistry>,
    confirmations: Arc<PendingConfirmationRegistry>,
    holder: String,
    state_path: PathBuf,
    cwd: String,
    configured_home_session_id: Option<String>,
    override_model: Option<String>,
    override_reasoning_mode: Option<ReasoningMode>,
}

impl ChannelControl {
    pub fn new(
        agent: Arc<AgentService>,
        leases: Arc<SessionLeaseRegistry>,
        confirmations: Arc<PendingConfirmationRegistry>,
        holder: impl Into<String>,
        state_dir: &Path,
        cwd: impl Into<String>,
        default_session_id: Option<&str>,
        override_model: Option<&str>,
        override_reasoning_mode: Option<ReasoningMode>,
    ) -> Self {
        Self {
            agent,
            leases,
            confirmations,
            holder: holder.into(),
            state_path: state_dir.join(CHANNEL_CONTROL_STATE_FILE),
            cwd: cwd.into(),
            configured_home_session_id: default_session_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            override_model: override_model
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string),
            override_reasoning_mode,
        }
    }

    pub async fn handle_command(&self, text: &str) -> Result<Option<String>> {
        let Some(command) = dwo::parse_channel_command(text) else {
            return Ok(None);
        };
        let reply = match command {
            DwoChannelCommand::Help => dwo::channel_command_help().to_string(),
            DwoChannelCommand::New => self.create_new_session().await?,
            DwoChannelCommand::List => self.render_session_list().await?,
            DwoChannelCommand::Load { session_ref } => self.load_topic(&session_ref).await?,
            DwoChannelCommand::SetHome { session_ref } => self.set_home(&session_ref).await?,
            DwoChannelCommand::Home => self.go_home().await?,
            DwoChannelCommand::Where => self.render_where().await?,
            DwoChannelCommand::Approve { confirmation_id } => {
                self.resolve_confirmation(&confirmation_id, PERMISSION_ALLOW_ONCE, None)?
            }
            DwoChannelCommand::Deny { confirmation_id } => {
                self.resolve_confirmation(&confirmation_id, PERMISSION_REJECT_ONCE, None)?
            }
            DwoChannelCommand::Cancel => self.cancel_current_session().await?,
            DwoChannelCommand::Usage(message) => message,
        };
        Ok(Some(reply))
    }

    pub async fn active_session(&self) -> Result<Arc<SessionAgent>> {
        let mut state = self.read_or_init_state()?;
        let home_session = self.ensure_home_session(&mut state).await?;
        let home_session_id = home_session.session_id().to_string();
        self.write_state(&mut state)?;

        let Some(active_session_id) = state.active_session_id else {
            return Ok(home_session);
        };
        if active_session_id == home_session_id {
            return Ok(home_session);
        }
        self.agent
            .load_session(&active_session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("active session not found: {active_session_id}"))
    }

    pub async fn run_prompt(
        &self,
        session_id: &str,
        user_input: Value,
        user_blocks: Vec<Value>,
        emit_update: UpdateEmitter,
        request_permission: PermissionRequester,
        extra_tool_schemas: Vec<Value>,
    ) -> Result<String> {
        self.leases.acquire(session_id, &self.holder).await?;
        let result = self
            .agent
            .run_prompt_with_extra_tools(
                session_id,
                user_input,
                user_blocks,
                emit_update,
                request_permission,
                extra_tool_schemas,
            )
            .await;
        self.leases
            .release_if_holder(session_id, &self.holder)
            .await;
        result
    }

    async fn ensure_home_session(
        &self,
        state: &mut ChannelControlState,
    ) -> Result<Arc<SessionAgent>> {
        if let Some(home_session_id) = state.home_session_id.clone() {
            if let Some(session) = self.agent.load_session(&home_session_id).await? {
                return Ok(session);
            }
            tracing::warn!(
                session_id = home_session_id,
                "channel home session was not found; resolving a replacement"
            );
            state.home_session_id = None;
        }

        if let Some(configured) = self.configured_home_session_id.as_deref() {
            let session = self.agent.load_session(configured).await?.ok_or_else(|| {
                anyhow::anyhow!("configured home session not found: {configured}")
            })?;
            state.home_session_id = Some(session.session_id().to_string());
            return Ok(session);
        }

        let session = self
            .agent
            .new_session_with_options(
                &self.cwd,
                self.override_model.as_deref(),
                self.override_reasoning_mode,
            )
            .await?;
        state.home_session_id = Some(session.session_id().to_string());
        Ok(session)
    }

    async fn create_new_session(&self) -> Result<String> {
        let mut state = self.read_or_init_state()?;
        self.ensure_home_session(&mut state).await?;

        let session = self
            .agent
            .new_session_with_options(
                &self.cwd,
                self.override_model.as_deref(),
                self.override_reasoning_mode,
            )
            .await?;
        state.active_session_id = Some(session.session_id().to_string());
        self.write_state(&mut state)?;
        Ok(format!(
            "已创建并进入新话题：{}\n返回主话题：/home",
            session.session_id()
        ))
    }

    async fn cancel_current_session(&self) -> Result<String> {
        let session = self.active_session().await?;
        let session_id = session.session_id().to_string();
        self.agent.cancel(&session_id).await;
        Ok(format!("已请求取消当前 session：{session_id}"))
    }

    async fn load_topic(&self, requested: &str) -> Result<String> {
        let mut state = self.read_or_init_state()?;
        let home_session = self.ensure_home_session(&mut state).await?;
        let home_session_id = home_session.session_id().to_string();
        let sessions = self.agent.list_sessions(None).await;
        let target = resolve_session_reference(&sessions, requested)?;
        self.agent
            .load_session(&target.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("找不到话题：{}", target.session_id))?;

        if target.session_id == home_session_id {
            state.active_session_id = None;
        } else {
            state.active_session_id = Some(target.session_id.clone());
        }
        self.write_state(&mut state)?;
        Ok(format!("已进入话题：{}", render_topic_label(&target)))
    }

    async fn set_home(&self, requested: &str) -> Result<String> {
        let mut state = self.read_or_init_state()?;
        let previous_home = self.ensure_home_session(&mut state).await?;
        let current_session_id = state
            .active_session_id
            .clone()
            .unwrap_or_else(|| previous_home.session_id().to_string());
        let sessions = self.agent.list_sessions(None).await;
        let target = resolve_session_reference(&sessions, requested)?;
        self.agent
            .load_session(&target.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("找不到话题：{}", target.session_id))?;

        state.home_session_id = Some(target.session_id.clone());
        state.active_session_id = if current_session_id == target.session_id {
            None
        } else {
            Some(current_session_id.clone())
        };
        self.write_state(&mut state)?;
        let mut reply = format!("已设置主话题：{}", render_topic_label(&target));
        if current_session_id == target.session_id {
            reply.push_str("\n当前已在主话题。");
        } else {
            let current = topic_label_by_id(&sessions, &current_session_id);
            reply.push_str(&format!("\n当前话题保持：{current}\n返回主话题：/home"));
        }
        Ok(reply)
    }

    async fn go_home(&self) -> Result<String> {
        let mut state = self.read_or_init_state()?;
        let home_session = self.ensure_home_session(&mut state).await?;
        let home_session_id = home_session.session_id().to_string();
        state.active_session_id = None;
        self.write_state(&mut state)?;
        let sessions = self.agent.list_sessions(None).await;
        Ok(format!(
            "已返回主话题：{}",
            topic_label_by_id(&sessions, &home_session_id)
        ))
    }

    async fn render_where(&self) -> Result<String> {
        let mut state = self.read_or_init_state()?;
        let home_session = self.ensure_home_session(&mut state).await?;
        let home_session_id = home_session.session_id().to_string();
        self.write_state(&mut state)?;
        let current_session_id = state
            .active_session_id
            .as_deref()
            .unwrap_or(&home_session_id);
        let holder = self
            .leases
            .holder(current_session_id)
            .await
            .unwrap_or_else(|| "none".to_string());
        let sessions = self.agent.list_sessions(None).await;
        Ok(format!(
            "当前话题：{}\n主话题：{}\n运行占用：{holder}",
            topic_label_by_id(&sessions, current_session_id),
            topic_label_by_id(&sessions, &home_session_id)
        ))
    }

    fn resolve_confirmation(
        &self,
        confirmation_id: &str,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<String> {
        let pending = self
            .confirmations
            .snapshot(confirmation_id)
            .ok_or_else(|| anyhow::anyhow!("找不到待确认请求：{confirmation_id}"))?;
        let session_holder = self.leases.holder_sync(&pending.session_id);
        let allowed = pending.requested_by == self.holder
            || session_holder
                .as_deref()
                .is_some_and(|holder| holder == self.holder);
        if !allowed {
            bail!(
                "当前 channel 无权处理确认 {}；session {} 当前占用者：{}",
                pending.confirmation_id,
                pending.session_id,
                session_holder.as_deref().unwrap_or("none")
            );
        }
        let snapshot =
            self.confirmations
                .resolve(confirmation_id, &self.holder, decision, reason)?;
        let mut reply = format!(
            "已{}确认：{}\nsession：{}\ntool：{}",
            if decision == PERMISSION_ALLOW_ONCE {
                "批准"
            } else {
                "拒绝"
            },
            snapshot.confirmation_id,
            snapshot.session_id,
            snapshot.tool_name
        );
        if let Some(reason) = reason.map(str::trim).filter(|value| !value.is_empty()) {
            reply.push_str("\nreason：");
            reply.push_str(reason);
        }
        Ok(reply)
    }

    async fn render_session_list(&self) -> Result<String> {
        let mut state = self.read_or_init_state()?;
        let home_session = self.ensure_home_session(&mut state).await?;
        let home_session_id = home_session.session_id().to_string();
        self.write_state(&mut state)?;
        let sessions = self.agent.list_sessions(None).await;
        if sessions.is_empty() {
            return Ok("暂无话题。".to_string());
        }
        let current_session_id = state
            .active_session_id
            .as_deref()
            .unwrap_or(&home_session_id);

        let mut lines = vec!["话题列表（* 当前，h 主话题）：".to_string()];
        for session in sessions.iter().take(30) {
            lines.push(render_session_line(
                session,
                current_session_id,
                &home_session_id,
            ));
        }
        if sessions.len() > 30 {
            lines.push(format!("... 还有 {} 个话题", sessions.len() - 30));
        }
        Ok(lines.join("\n"))
    }

    pub fn confirmation_message(snapshot: &PendingConfirmationSnapshot) -> String {
        let args = truncate_json(&snapshot.raw_input, 320);
        format!(
            "[confirm required]\nsession: {}\nconfirm: {}\ntool: {}\nargs: {}\n/approve {}\n/deny {}",
            snapshot.session_id,
            snapshot.confirmation_id,
            snapshot.tool_name,
            args,
            snapshot.confirmation_id,
            snapshot.confirmation_id
        )
    }

    fn read_or_init_state(&self) -> Result<ChannelControlState> {
        if self.state_path.is_file() {
            let text = read_utf8_text(&self.state_path)?;
            let state: ChannelControlState = serde_yaml::from_str(&text)
                .with_context(|| format!("parse {}", self.state_path.display()))?;
            return Ok(state);
        }
        Ok(ChannelControlState {
            home_session_id: None,
            active_session_id: None,
            updated_at: None,
        })
    }

    fn write_state(&self, state: &mut ChannelControlState) -> Result<()> {
        state.updated_at = Some(utc_iso());
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let text = serde_yaml::to_string(state)?;
        std::fs::write(&self.state_path, text)
            .with_context(|| format!("write {}", self.state_path.display()))?;
        Ok(())
    }
}

fn render_session_line(
    session: &SessionMetaPayload,
    current_session_id: &str,
    home_session_id: &str,
) -> String {
    let marker = match (
        session.session_id == current_session_id,
        session.session_id == home_session_id,
    ) {
        (true, true) => "*h",
        (true, false) => "*",
        (false, true) => "h",
        (false, false) => "-",
    };
    let title = session
        .title
        .as_deref()
        .map(compact_inline_text)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());
    let running = if session_is_running(session.state) {
        "yes"
    } else {
        "no"
    };
    format!(
        "{marker} {title} | id: {} | running: {running}",
        session.session_id
    )
}

fn resolve_session_reference(
    sessions: &[SessionMetaPayload],
    requested: &str,
) -> Result<SessionMetaPayload> {
    let requested = compact_inline_text(requested);
    if requested.is_empty() {
        bail!("话题名称或 id 不能为空");
    }
    if let Some(session) = sessions
        .iter()
        .find(|session| session.session_id == requested)
    {
        return Ok(session.clone());
    }

    let matches = sessions
        .iter()
        .filter(|session| {
            session
                .title
                .as_deref()
                .map(compact_inline_text)
                .is_some_and(|title| title == requested)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [session] => Ok((*session).clone()),
        [] => bail!("找不到话题：{requested}。使用 /list 查看名称和 id"),
        many => {
            let ids = many
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("话题名称不唯一：{requested}。请使用 id：{ids}")
        }
    }
}

fn render_topic_label(session: &SessionMetaPayload) -> String {
    let title = session
        .title
        .as_deref()
        .map(compact_inline_text)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());
    format!("{title} ({})", session.session_id)
}

fn topic_label_by_id(sessions: &[SessionMetaPayload], session_id: &str) -> String {
    sessions
        .iter()
        .find(|session| session.session_id == session_id)
        .map(render_topic_label)
        .unwrap_or_else(|| session_id.to_string())
}

fn session_is_running(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Running | AgentState::WaitingUserConfirm | AgentState::Cancelling
    )
}

fn compact_inline_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_json(value: &Value, limit: usize) -> String {
    let raw = serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".to_string());
    if raw.chars().count() <= limit {
        return raw;
    }
    let mut out = String::new();
    for (index, ch) in raw.chars().enumerate() {
        if index >= limit {
            out.push_str("...<truncated>");
            break;
        }
        out.push(ch);
    }
    out
}

fn permission_decision_with_reason(decision: &str, reason: Option<&str>) -> String {
    if decision != PERMISSION_REJECT_ONCE {
        return decision.to_string();
    }
    match reason.map(str::trim).filter(|value| !value.is_empty()) {
        Some(reason) => format!("{PERMISSION_REJECT_ONCE}:{reason}"),
        None => PERMISSION_REJECT_ONCE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::{AgentTools, PolicyMode, ReasoningMode};
    use tempfile::tempdir;

    #[tokio::test]
    async fn leases_are_single_holder() {
        let registry = SessionLeaseRegistry::new();

        registry.acquire("s1", "weixin:user").await.unwrap();
        registry.acquire("s1", "weixin:user").await.unwrap();
        let err = registry.acquire("s1", "feishu:user").await.unwrap_err();
        assert!(err.to_string().contains("occupied"));

        assert!(registry.release_if_holder("s1", "weixin:user").await);
        registry.acquire("s1", "feishu:user").await.unwrap();
    }

    #[test]
    fn channel_control_state_round_trips() {
        let tmp = tempdir().unwrap();
        let state_path = tmp.path().join(CHANNEL_CONTROL_STATE_FILE);
        let mut state = ChannelControlState {
            home_session_id: Some("home".to_string()),
            active_session_id: Some("other".to_string()),
            updated_at: None,
        };
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        state.updated_at = Some(utc_iso());
        std::fs::write(&state_path, serde_yaml::to_string(&state).unwrap()).unwrap();
        let loaded: ChannelControlState =
            serde_yaml::from_str(&read_utf8_text(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.home_session_id.as_deref(), Some("home"));
        assert_eq!(loaded.active_session_id.as_deref(), Some("other"));
    }

    #[test]
    fn channel_control_state_reads_legacy_default_as_home() {
        let loaded: ChannelControlState =
            serde_yaml::from_str("default_session_id: legacy-home\nactive_session_id: other\n")
                .unwrap();

        assert_eq!(loaded.home_session_id.as_deref(), Some("legacy-home"));
        assert_eq!(loaded.active_session_id.as_deref(), Some("other"));
    }

    #[test]
    fn session_list_line_shows_id_title_and_running_status() {
        let session = session_meta("s1", Some("Bug fix"), AgentState::Running);

        let line = render_session_line(&session, "s1", "home");

        assert!(line.starts_with("* "));
        assert!(line.contains("id: s1"));
        assert!(line.contains("Bug fix"));
        assert!(line.contains("running: yes"));
    }

    #[test]
    fn session_list_line_marks_current_home() {
        let session = session_meta("s1", Some("Main topic"), AgentState::Idle);

        let line = render_session_line(&session, "s1", "s1");

        assert!(line.starts_with("*h "));
    }

    #[test]
    fn session_reference_resolves_exact_id_or_unique_title() {
        let sessions = vec![
            session_meta("s1", Some("Bug fix"), AgentState::Idle),
            session_meta("Bug fix", Some("Another topic"), AgentState::Idle),
        ];

        assert_eq!(
            resolve_session_reference(&sessions, "s1")
                .unwrap()
                .session_id,
            "s1"
        );
        assert_eq!(
            resolve_session_reference(&sessions, "Another topic")
                .unwrap()
                .session_id,
            "Bug fix"
        );
        assert_eq!(
            resolve_session_reference(&sessions, "Bug fix")
                .unwrap()
                .session_id,
            "Bug fix",
            "an exact id wins over a title"
        );
    }

    #[test]
    fn session_reference_rejects_ambiguous_title() {
        let sessions = vec![
            session_meta("s1", Some("Same name"), AgentState::Idle),
            session_meta("s2", Some("Same name"), AgentState::Idle),
        ];

        let err = resolve_session_reference(&sessions, "Same name").unwrap_err();
        assert!(err.to_string().contains("s1, s2"));
    }

    #[tokio::test]
    async fn confirmation_registry_resolves_waiter_and_writes_audit() {
        let tmp = tempdir().unwrap();
        let registry = PendingConfirmationRegistry::new(tmp.path());
        let (snapshot, rx) = registry
            .create(
                "s1",
                json!({
                    "tool_call_id": "call_1",
                    "title": "terminal_exec",
                    "raw_input": {"command": "cargo check"}
                })
                .as_object()
                .cloned()
                .unwrap(),
                "feishu:dm:u1",
            )
            .unwrap();

        registry
            .resolve(
                &snapshot.confirmation_id,
                "feishu:dm:u1",
                PERMISSION_ALLOW_ONCE,
                None,
            )
            .unwrap();

        assert_eq!(rx.await.unwrap(), PERMISSION_ALLOW_ONCE);
        assert!(registry.audit_path.is_file());
    }

    #[test]
    fn permission_decision_with_reason_encodes_rejection_reason() {
        assert_eq!(
            permission_decision_with_reason(PERMISSION_REJECT_ONCE, Some("try read-only")),
            "reject_once:try read-only"
        );
        assert_eq!(
            permission_decision_with_reason(PERMISSION_ALLOW_ONCE, Some("ignored")),
            PERMISSION_ALLOW_ONCE
        );
    }

    fn session_meta(id: &str, title: Option<&str>, state: AgentState) -> SessionMetaPayload {
        SessionMetaPayload {
            session_id: id.to_string(),
            cwd: ".".to_string(),
            title: title.map(str::to_string),
            model_id: "model".to_string(),
            mode_id: PolicyMode::Confirm,
            state,
            stop_reason: None,
            updated_at: None,
            max_running_turn: None,
            runtime_tools: AgentTools::default(),
            tool_schemas: Vec::new(),
            pending_model_id: None,
            reasoning_mode: ReasoningMode::Auto,
            pending_reasoning_mode: None,
        }
    }
}
