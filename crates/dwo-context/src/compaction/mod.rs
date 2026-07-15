mod plan;
mod tools;

pub use plan::{
    CompactionPlan, CompactionPlanner, CompactionView, DEFAULT_RECENT_TURNS,
    DEFAULT_RECENT_USER_BYTES,
};

use tools::compact_tool_exchanges;

const OMITTED_MARKER: &str = "\n... content omitted ...\n";

pub const COMPACT_INSTRUCTION: &str = r#"Summarize the conversation for another model that will continue the work.

Preserve user requirements, architectural decisions, completed work, relevant file paths and identifiers, current state, unresolved problems, and concrete next steps. Be precise and compact. Do not repeat transient runtime notices, permissions, hidden reasoning, or raw tool output when its result can be stated directly."#;
