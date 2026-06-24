//! Tool runtimes and session management.

pub(crate) mod builtin;
pub mod schema;
pub mod session;
pub(crate) mod session_creator;
pub(crate) mod session_manager;
pub(crate) mod tool_catalog;
pub(crate) mod tool_output;
pub mod tool_run_manager;

pub use builtin::channel::{
    FEISHU_REPLY_CARD_TOOL, FEISHU_REPLY_MEDIA_TOOL, FeishuReplyCardResult, FeishuReplyMediaKind,
    FeishuReplyMediaResult, FeishuToolBridge, FeishuToolExecutor, WEIXIN_REPLY_MEDIA_TOOL,
    WeixinReplyMediaResult, WeixinToolBridge, WeixinToolExecutor, feishu_tool_schemas,
    has_weixin_reply_media_tool, weixin_tool_schemas,
};
pub use builtin::{
    FileEditError, PermissionRequester, SendSubagentPayload, SpawnSubagentPayload, StateSetter,
    SubagentExecutor, SubagentIdPayload, TerminalExecutor, TerminalHandle, TerminalSession,
    ToolExecutionContext, UpdateEmitter, WaitTarget, file_edit_text, parse_wait_target,
    subagent_not_found, terminal_not_found, wait_seconds, wait_session,
};
pub use schema::{tool_schemas, tool_schemas_from_templates};
pub use session::{Cap, ToolArgs, ToolSession};
pub use tool_run_manager::{ChannelToolExecutor, ToolRunManager};
