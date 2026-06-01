//! Embedded prompt templates.
//!
//! Each asset is embedded via `include_str!` so the binary ships without a
//! runtime dependency on the on-disk template tree.

pub const TOOLS_PROMPT_ROOT: &str = include_str!("tools_prompt.md");

pub mod codemode {
    pub const TOOLS_PROMPT: &str = include_str!("codemode/tools_prompt.md");
    pub const TOOL_SCHEMA: &str = include_str!("codemode/tool_schema.json");
}

pub mod compact {
    pub const COMPACT_PROMPT: &str = include_str!("compact/compact_prompt.md");
    pub const SUMMARY_PREFIX: &str = include_str!("compact/summary_prefix.md");
}

pub mod channel {
    pub mod weixin {
        pub const TOOL_SCHEMA: &str = include_str!("channel/weixin/tool_schema.json");
    }
}

pub mod files {
    pub const TOOLS_PROMPT: &str = include_str!("files/tools_prompt.md");
    pub const TOOL_SCHEMA: &str = include_str!("files/tool_schema.json");
}

pub mod subagent {
    pub const TOOLS_PROMPT: &str = include_str!("subagent/tools_prompt.md");
    pub const TOOL_SCHEMA: &str = include_str!("subagent/tool_schema.json");
}

pub mod terminal {
    pub const TOOLS_PROMPT: &str = include_str!("terminal/tools_prompt.md");
    pub const TOOL_SCHEMA: &str = include_str!("terminal/tool_schema.json");
}
