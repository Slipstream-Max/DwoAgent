//! Tool runtimes and session management.

pub mod file_edit_runtime;
pub mod schema;
pub mod session;
pub mod subagent_tool_runtime;
pub mod terminal_runtime;
pub mod tool_run_manager;
pub mod weixin_runtime;

pub use file_edit_runtime::{FileEditError, file_edit_text};
pub use schema::{tool_schemas, tool_schemas_from_templates};
pub use session::{Cap, ToolArgs, ToolSession};
pub use subagent_tool_runtime::{
    PermissionRequester, SendSubagentPayload, SpawnSubagentPayload, StateSetter, SubagentIdPayload,
    ToolExecutionContext, UpdateEmitter, WaitSubagentPayload, subagent_not_found,
};
pub use terminal_runtime::{TerminalExecutor, TerminalHandle, TerminalSession, terminal_not_found};
pub use tool_run_manager::{ChannelToolExecutor, SubagentExecutor, ToolRunManager};
pub use weixin_runtime::{
    WEIXIN_REPLY_MEDIA_TOOL, WeixinReplyMediaResult, WeixinToolBridge, WeixinToolExecutor,
    has_weixin_reply_media_tool, weixin_tool_schemas,
};
