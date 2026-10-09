//! Tool execution primitives shared by every dwoagent ingress.

pub mod call;
mod exec;
pub mod file_edit;
pub mod images;
pub mod manager;
pub mod policy;
pub mod prompt;
pub mod questionnaire;
mod read_file;
pub mod result;
pub mod schema;
mod telemetry;
pub mod terminal;
mod text_encoding;
pub mod todo;

pub use call::{ParsedToolCall, ToolCall, ToolCallParseError, ToolIntent};
pub use file_edit::FileEditManager;
pub use manager::{
    ConfirmationDecision, ConfirmationHandler, ConfirmationRequest, ExecutionContext, ToolManager,
};
pub use policy::{Authorization, CommandRule, PolicyConfig, SessionMode, ToolPolicyEngine};
pub use questionnaire::{
    QuestionnaireHandler, QuestionnaireQuestion, QuestionnaireRequest, QuestionnaireResponse,
};
pub use result::ToolResult;
pub use telemetry::{ToolEvent, ToolEventHandler};
pub use terminal::{TerminalId, TerminalManager};
pub use todo::{
    TodoAction, TodoEntry, TodoEntryPriority, TodoEntryStatus, TodoHandler, TodoRequest,
    TodoResponse,
};
