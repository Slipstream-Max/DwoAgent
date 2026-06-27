//! Core-side handling for gateway-delivered ingress events.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::{ContentBlock, TextContent};
use anyhow::Result;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use super::channel_control::ChannelControl;
use super::channel_input::{
    append_channel_context, file_uri_from_path, image_url_block_from_file,
    image_url_block_from_path, resolve_config_path, resource_link_block, sanitize_filename_or,
};
use super::config::{
    FeishuAccessPolicy, FeishuChannelConfig, WeixinChannelConfig, load_channel_runtime_config,
};
use super::response::ChannelUpdateCollector;
use crate::agent::constants::PERMISSION_REJECT_ONCE;
use crate::agent::service::AgentService;
use crate::protocol::acp::mapper;
use crate::protocol::dwo::{
    DwoIngressAttachment, DwoIngressChannel, DwoIngressEvent, DwoOutboundAction, DwoOutboundBody,
};
use crate::tools::{
    FeishuReplyCardResult, FeishuReplyMediaKind, FeishuReplyMediaResult, FeishuToolBridge,
    FeishuToolExecutor, PermissionRequester, WEIXIN_REPLY_MEDIA_TOOL, WeixinReplyMediaResult,
    WeixinToolBridge, WeixinToolExecutor, feishu_tool_schemas, weixin_tool_schemas,
};

const FEISHU_STATE_SUBDIR: &str = "feishu";
const WEIXIN_STATE_SUBDIR: &str = "weixin";

pub async fn handle_ingress_event(
    agent: Arc<AgentService>,
    event: DwoIngressEvent,
) -> Result<Vec<DwoOutboundAction>> {
    let config = load_channel_runtime_config(agent.agent_structure_dir())?;
    match event.channel {
        DwoIngressChannel::Weixin => handle_weixin_event(agent, event, &config.weixin).await,
        DwoIngressChannel::Feishu => handle_feishu_event(agent, event, &config.feishu).await,
    }
}

async fn handle_weixin_event(
    agent: Arc<AgentService>,
    event: DwoIngressEvent,
    config: &WeixinChannelConfig,
) -> Result<Vec<DwoOutboundAction>> {
    if !config.enabled {
        return Ok(Vec::new());
    }

    let target = reply_target(&event);
    let holder = event
        .conversation
        .holder
        .clone()
        .unwrap_or_else(|| format!("weixin:user:{}", event.source.id));
    let state_dir = agent.channel_state_dir().join(WEIXIN_STATE_SUBDIR);
    let workspace_dir = resolve_config_path(agent.agent_structure_dir(), &config.workspace_dir);
    let channel_control = ChannelControl::new(
        agent.clone(),
        agent.session_leases(),
        agent.pending_confirmations(),
        holder.clone(),
        &state_dir,
        workspace_dir.to_string_lossy().to_string(),
        config.default_session_id.as_deref(),
        config.override_model.as_deref(),
        config.override_reasoning_mode,
    );

    if let Some(text) = command_text(&event)
        && let Some(reply) = channel_control.handle_command(text).await?
    {
        return Ok(vec![text_action(DwoIngressChannel::Weixin, target, reply)]);
    }

    let session = channel_control.active_session().await?;
    let Some((user_input, user_blocks)) =
        build_weixin_user_input(&event, config, session.session_dir())?
    else {
        return Ok(Vec::new());
    };

    let action_collector = OutboundActionCollector::new(DwoIngressChannel::Weixin, target.clone());
    let tool_manager = session.tool_manager().await;
    if config.media_output {
        let bridge = Arc::new(RecordingWeixinBridge {
            actions: action_collector.clone(),
        });
        let executor = Arc::new(WeixinToolExecutor::new(
            bridge,
            vec![workspace_dir.clone(), session.session_dir().to_path_buf()],
        ));
        tool_manager.set_channel_tool_executor(Some(executor)).await;
    }

    let update_collector = ChannelUpdateCollector::new(config.response_detail);
    let run_result = channel_control
        .run_prompt(
            session.session_id(),
            user_input,
            user_blocks,
            update_collector.emitter(),
            rejecting_channel_permission_requester(),
            if config.media_output {
                weixin_tool_schemas()
            } else {
                Vec::new()
            },
        )
        .await;
    if config.media_output {
        tool_manager.set_channel_tool_executor(None).await;
    }
    run_result?;

    let mut actions = action_collector.take().await;
    append_collected_text_actions(
        &mut actions,
        DwoIngressChannel::Weixin,
        target,
        update_collector.finish().await,
    );
    Ok(actions)
}

async fn handle_feishu_event(
    agent: Arc<AgentService>,
    event: DwoIngressEvent,
    config: &FeishuChannelConfig,
) -> Result<Vec<DwoOutboundAction>> {
    if !config.enabled || !is_feishu_allowed(&event, config) {
        return Ok(Vec::new());
    }

    let target = reply_target(&event);
    let chat_kind = event
        .conversation
        .kind
        .as_deref()
        .unwrap_or("direct")
        .to_string();
    let state_key = event
        .conversation
        .state_key
        .as_deref()
        .unwrap_or(&event.conversation.id);
    let holder = event
        .conversation
        .holder
        .clone()
        .unwrap_or_else(|| format!("feishu:{chat_kind}:{state_key}"));
    let state_dir = agent
        .channel_state_dir()
        .join(FEISHU_STATE_SUBDIR)
        .join(sanitize_filename_or(&chat_kind, "direct"))
        .join(sanitize_filename_or(state_key, "unknown"));
    let workspace_dir = resolve_config_path(agent.agent_structure_dir(), &config.workspace_dir);
    let channel_control = ChannelControl::new(
        agent.clone(),
        agent.session_leases(),
        agent.pending_confirmations(),
        holder.clone(),
        &state_dir,
        workspace_dir.to_string_lossy().to_string(),
        config.default_session_id.as_deref(),
        config.override_model.as_deref(),
        config.override_reasoning_mode,
    );

    if let Some(text) = command_text(&event)
        && let Some(reply) = channel_control.handle_command(text).await?
    {
        return Ok(vec![text_action(DwoIngressChannel::Feishu, target, reply)]);
    }

    let session = channel_control.active_session().await?;
    let Some((user_input, user_blocks)) =
        build_feishu_user_input(&event, config, session.session_dir())?
    else {
        return Ok(Vec::new());
    };

    let action_collector = OutboundActionCollector::new(DwoIngressChannel::Feishu, target.clone());
    let expose_output_tools = config.media_output || config.card_output;
    let tool_manager = session.tool_manager().await;
    if expose_output_tools {
        let bridge = Arc::new(RecordingFeishuBridge {
            actions: action_collector.clone(),
        });
        let executor = Arc::new(FeishuToolExecutor::new(
            bridge,
            vec![workspace_dir.clone(), session.session_dir().to_path_buf()],
            config.media_output,
            config.card_output,
        ));
        tool_manager.set_channel_tool_executor(Some(executor)).await;
    }

    let update_collector = ChannelUpdateCollector::new(config.response_detail);
    let run_result = channel_control
        .run_prompt(
            session.session_id(),
            user_input,
            user_blocks,
            update_collector.emitter(),
            rejecting_channel_permission_requester(),
            feishu_tool_schemas(config.media_output, config.card_output),
        )
        .await;
    if expose_output_tools {
        tool_manager.set_channel_tool_executor(None).await;
    }
    run_result?;

    let mut actions = action_collector.take().await;
    append_collected_text_actions(
        &mut actions,
        DwoIngressChannel::Feishu,
        target,
        update_collector.finish().await,
    );
    Ok(actions)
}

fn build_weixin_user_input(
    event: &DwoIngressEvent,
    config: &WeixinChannelConfig,
    _session_dir: &Path,
) -> Result<Option<(Value, Vec<Value>)>> {
    let mut blocks = Vec::new();
    push_text_block(&mut blocks, event.text.as_deref());
    push_attachment_blocks(&mut blocks, &event.attachments, config.media_input)?;
    if blocks.is_empty() {
        if event_has_media(event) && !config.media_input {
            let text = "当前微信通道未开启媒体输入。";
            return Ok(Some(mapper::normalize_prompt_blocks(&[
                ContentBlock::Text(TextContent::new(text)),
            ])?));
        }
        return Ok(None);
    }
    let instructions = if config.media_output {
        vec!["本轮如需发送本地文件或图片，请使用 weixin_reply_media 回复当前微信对话。"]
    } else {
        Vec::new()
    };
    append_channel_context(&mut blocks, "Weixin", &instructions);
    Ok(Some(mapper::normalize_prompt_blocks(&blocks)?))
}

fn build_feishu_user_input(
    event: &DwoIngressEvent,
    config: &FeishuChannelConfig,
    _session_dir: &Path,
) -> Result<Option<(Value, Vec<Value>)>> {
    let mut blocks = Vec::new();
    let text = event.text.as_deref().map(|text| {
        if event.conversation.kind.as_deref() == Some("group") {
            format!(
                "Feishu group message from {}:\n{text}",
                event.source.name.as_deref().unwrap_or(&event.source.id)
            )
        } else {
            text.to_string()
        }
    });
    push_text_block(&mut blocks, text.as_deref());
    push_attachment_blocks(&mut blocks, &event.attachments, config.media_input)?;
    if blocks.is_empty() {
        if event_has_media(event) && !config.media_input {
            let text = "当前飞书通道未开启媒体输入。";
            return Ok(Some(mapper::normalize_prompt_blocks(&[
                ContentBlock::Text(TextContent::new(text)),
            ])?));
        }
        return Ok(None);
    }
    let mut instructions = Vec::new();
    if config.media_output {
        instructions
            .push("本轮如需发送本地文件或图片，请使用 feishu_reply_media 回复当前飞书对话。");
    }
    if config.card_output {
        instructions.push("本轮如需发送飞书交互卡片，请使用 feishu_reply_card 回复当前飞书对话。");
    }
    append_channel_context(&mut blocks, "Feishu", &instructions);
    Ok(Some(mapper::normalize_prompt_blocks(&blocks)?))
}

fn push_text_block(blocks: &mut Vec<ContentBlock>, text: Option<&str>) {
    if let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) {
        blocks.push(ContentBlock::Text(TextContent::new(text)));
    }
}

fn push_attachment_blocks(
    blocks: &mut Vec<ContentBlock>,
    attachments: &[DwoIngressAttachment],
    enabled: bool,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    for attachment in attachments {
        let path = &attachment.path;
        if !path.is_file() {
            continue;
        }
        let uri = file_uri_from_path(path);
        let name = attachment
            .name
            .as_deref()
            .or_else(|| path.file_name().and_then(|name| name.to_str()));
        let mime_type = attachment.mime_type.as_deref();
        blocks.push(resource_link_block(&uri, name, mime_type)?);
        let image_block = match mime_type {
            Some(mime_type) => image_url_block_from_file(path, mime_type)?,
            None => image_url_block_from_path(path)?,
        };
        if let Some(image_block) = image_block {
            blocks.push(image_block);
        }
    }
    Ok(())
}

fn append_collected_text_actions(
    actions: &mut Vec<DwoOutboundAction>,
    channel: DwoIngressChannel,
    target: String,
    collected: super::response::ChannelCollectedUpdates,
) {
    if let Some(detail) = collected.detail_text.filter(|text| !text.is_empty()) {
        actions.push(text_action(channel.clone(), target.clone(), detail));
    }
    if !collected.response_text.is_empty() {
        actions.push(text_action(channel, target, collected.response_text));
    }
}

fn text_action(
    channel: DwoIngressChannel,
    target: String,
    text: impl Into<String>,
) -> DwoOutboundAction {
    DwoOutboundAction {
        channel,
        target,
        body: DwoOutboundBody::Text { text: text.into() },
    }
}

fn reply_target(event: &DwoIngressEvent) -> String {
    event
        .conversation
        .reply_to
        .as_deref()
        .unwrap_or(&event.conversation.id)
        .to_string()
}

fn command_text(event: &DwoIngressEvent) -> Option<&str> {
    event
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| text.starts_with('/'))
}

fn event_has_media(event: &DwoIngressEvent) -> bool {
    !event.attachments.is_empty()
        || event
            .raw
            .get("has_media")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn is_feishu_allowed(event: &DwoIngressEvent, config: &FeishuChannelConfig) -> bool {
    let is_group = event.conversation.kind.as_deref() == Some("group");
    let policy = if is_group {
        config.group_policy
    } else {
        config.dm_policy
    };
    if policy == FeishuAccessPolicy::AllowAll {
        return true;
    }
    let allow_list = if is_group {
        &config.group_allow_from
    } else {
        &config.allow_from
    };
    allow_list
        .iter()
        .any(|item| item == "*" || item == &event.source.id)
        || allow_list.iter().any(|item| item == &event.conversation.id)
}

fn rejecting_channel_permission_requester() -> PermissionRequester {
    Arc::new(move |session_id: String, payload: Map<String, Value>| {
        Box::pin(async move {
            let _ = (session_id, payload);
            Ok(PERMISSION_REJECT_ONCE.to_string())
        })
    })
}

#[derive(Clone)]
struct OutboundActionCollector {
    channel: DwoIngressChannel,
    target: String,
    actions: Arc<Mutex<Vec<DwoOutboundAction>>>,
}

impl OutboundActionCollector {
    fn new(channel: DwoIngressChannel, target: String) -> Self {
        Self {
            channel,
            target,
            actions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn push_media(&self, path: PathBuf, kind: Option<String>, file_type: Option<String>) {
        self.actions.lock().await.push(DwoOutboundAction {
            channel: self.channel.clone(),
            target: self.target.clone(),
            body: DwoOutboundBody::Media {
                path,
                kind,
                file_type,
            },
        });
    }

    async fn push_card(&self, card: Value) {
        self.actions.lock().await.push(DwoOutboundAction {
            channel: self.channel.clone(),
            target: self.target.clone(),
            body: DwoOutboundBody::Card { card },
        });
    }

    async fn take(&self) -> Vec<DwoOutboundAction> {
        std::mem::take(&mut *self.actions.lock().await)
    }
}

struct RecordingWeixinBridge {
    actions: OutboundActionCollector,
}

#[async_trait::async_trait]
impl WeixinToolBridge for RecordingWeixinBridge {
    async fn reply_media(&self, path: &Path) -> Result<WeixinReplyMediaResult> {
        self.actions
            .push_media(path.to_path_buf(), Some("auto".to_string()), None)
            .await;
        Ok(WeixinReplyMediaResult {
            message_id: format!("{WEIXIN_REPLY_MEDIA_TOOL}:deferred"),
        })
    }
}

struct RecordingFeishuBridge {
    actions: OutboundActionCollector,
}

#[async_trait::async_trait]
impl FeishuToolBridge for RecordingFeishuBridge {
    async fn reply_media(
        &self,
        path: &Path,
        kind: FeishuReplyMediaKind,
        file_type: Option<&str>,
    ) -> Result<FeishuReplyMediaResult> {
        self.actions
            .push_media(
                path.to_path_buf(),
                Some(feishu_media_kind_name(kind).to_string()),
                file_type.map(str::to_string),
            )
            .await;
        Ok(FeishuReplyMediaResult {
            message_id: "feishu_reply_media:deferred".to_string(),
            resource_key: "deferred".to_string(),
            msg_type: "deferred".to_string(),
        })
    }

    async fn reply_card(&self, card: Value) -> Result<FeishuReplyCardResult> {
        self.actions.push_card(card).await;
        Ok(FeishuReplyCardResult {
            message_id: "feishu_reply_card:deferred".to_string(),
        })
    }
}

fn feishu_media_kind_name(kind: FeishuReplyMediaKind) -> &'static str {
    match kind {
        FeishuReplyMediaKind::Auto => "auto",
        FeishuReplyMediaKind::Image => "image",
        FeishuReplyMediaKind::File => "file",
    }
}
