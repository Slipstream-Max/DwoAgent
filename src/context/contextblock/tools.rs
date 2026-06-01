//! Build the `<tools>` context block.

use super::xml::{block, text_block};
use crate::config::models::AgentTools;
use crate::templates;

pub fn build_tools(tools: &AgentTools) -> String {
    let mut chunks = vec![templates::TOOLS_PROMPT_ROOT.trim().to_string()];
    if tools.mcp_enabled() {
        chunks.push(text_block(
            "codemode",
            templates::codemode::TOOLS_PROMPT.trim(),
        ));
    }
    if tools.file_edit_enabled() {
        chunks.push(text_block("files", templates::files::TOOLS_PROMPT.trim()));
    }
    if tools.terminal_enabled() {
        chunks.push(text_block(
            "terminal",
            templates::terminal::TOOLS_PROMPT.trim(),
        ));
    }
    if tools.subagent_enabled() {
        chunks.push(text_block(
            "subagent",
            templates::subagent::TOOLS_PROMPT.trim(),
        ));
    }
    block("tools", &chunks.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::ToolSwitch;

    #[test]
    fn build_tools_omits_disabled_sections() {
        let tools = AgentTools {
            mcp: ToolSwitch::Disable,
            file_edit: ToolSwitch::Enable,
            terminal: ToolSwitch::Disable,
            subagent: ToolSwitch::Disable,
        };

        let prompt = build_tools(&tools);

        assert!(prompt.contains("<files>"));
        assert!(!prompt.contains("<codemode>"));
        assert!(!prompt.contains("<terminal>"));
        assert!(!prompt.contains("<subagent>"));
    }
}
