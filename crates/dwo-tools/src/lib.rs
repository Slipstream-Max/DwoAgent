//! Tool execution primitives shared by every dwoagent ingress.

pub mod call;
mod exec;
pub mod file_edit;
pub mod manager;
pub mod policy;
pub mod prompt;
mod read_file;
pub mod result;
pub mod schema;
mod telemetry;
pub mod terminal;

pub use call::{HandoffArgs, ParsedToolCall, ToolCall, ToolCallParseError, ToolIntent};
pub use file_edit::FileEditManager;
pub use manager::{
    ConfirmationDecision, ConfirmationHandler, ConfirmationRequest, ExecutionContext, ToolManager,
};
pub use policy::{Authorization, CommandRule, PolicyConfig, SessionMode, ToolPolicyEngine};
pub use result::ToolResult;
pub use telemetry::{ToolEvent, ToolEventHandler};
pub use terminal::{TerminalId, TerminalManager};
