use crate::config::models::AgentTools;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolRoute {
    Immediate,
    CreateSession,
    ListSessions,
    Wait,
    OperateSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolGate {
    FileEdit,
    Terminal,
    Subagent,
    TerminalOrSubagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolSpec {
    pub name: &'static str,
    pub kind: &'static str,
    pub route: ToolRoute,
    gate: ToolGate,
    is_file_write: bool,
}

impl ToolSpec {
    pub(crate) fn is_enabled(self, tools: &AgentTools) -> bool {
        match self.gate {
            ToolGate::FileEdit => tools.file_edit_enabled(),
            ToolGate::Terminal => tools.terminal_enabled(),
            ToolGate::Subagent => tools.subagent_enabled(),
            ToolGate::TerminalOrSubagent => tools.terminal_enabled() || tools.subagent_enabled(),
        }
    }

    pub(crate) fn is_file_write(self) -> bool {
        self.is_file_write
    }
}

const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "file_edit",
        kind: "file_edit",
        route: ToolRoute::Immediate,
        gate: ToolGate::FileEdit,
        is_file_write: true,
    },
    ToolSpec {
        name: "text_replace",
        kind: "file_edit",
        route: ToolRoute::Immediate,
        gate: ToolGate::FileEdit,
        is_file_write: true,
    },
    ToolSpec {
        name: "write_file",
        kind: "file_edit",
        route: ToolRoute::Immediate,
        gate: ToolGate::FileEdit,
        is_file_write: true,
    },
    ToolSpec {
        name: "terminal_exec",
        kind: "terminal",
        route: ToolRoute::CreateSession,
        gate: ToolGate::Terminal,
        is_file_write: false,
    },
    ToolSpec {
        name: "list_terminals",
        kind: "terminal",
        route: ToolRoute::ListSessions,
        gate: ToolGate::Terminal,
        is_file_write: false,
    },
    ToolSpec {
        name: "terminal_checkout",
        kind: "terminal",
        route: ToolRoute::OperateSession,
        gate: ToolGate::Terminal,
        is_file_write: false,
    },
    ToolSpec {
        name: "terminal_kill",
        kind: "terminal",
        route: ToolRoute::OperateSession,
        gate: ToolGate::Terminal,
        is_file_write: false,
    },
    ToolSpec {
        name: "spawn_subagent",
        kind: "subagent",
        route: ToolRoute::CreateSession,
        gate: ToolGate::Subagent,
        is_file_write: false,
    },
    ToolSpec {
        name: "list_subagents",
        kind: "subagent",
        route: ToolRoute::ListSessions,
        gate: ToolGate::Subagent,
        is_file_write: false,
    },
    ToolSpec {
        name: "checkout_subagent",
        kind: "subagent",
        route: ToolRoute::OperateSession,
        gate: ToolGate::Subagent,
        is_file_write: false,
    },
    ToolSpec {
        name: "send_subagent",
        kind: "subagent",
        route: ToolRoute::OperateSession,
        gate: ToolGate::Subagent,
        is_file_write: false,
    },
    ToolSpec {
        name: "close_subagent",
        kind: "subagent",
        route: ToolRoute::OperateSession,
        gate: ToolGate::Subagent,
        is_file_write: false,
    },
    ToolSpec {
        name: "wait",
        kind: "wait",
        route: ToolRoute::Wait,
        gate: ToolGate::TerminalOrSubagent,
        is_file_write: false,
    },
];

pub(crate) fn lookup_tool(tool_name: &str) -> Option<ToolSpec> {
    let tool_name = tool_name.trim();
    TOOL_SPECS
        .iter()
        .copied()
        .find(|spec| spec.name == tool_name)
}

pub(crate) fn tool_kind(tool_name: &str) -> &'static str {
    lookup_tool(tool_name)
        .map(|spec| spec.kind)
        .unwrap_or_else(|| match tool_name.trim() {
            "feishu_reply_media" | "feishu_reply_card" | "weixin_reply_media" => "channel",
            _ => "unknown",
        })
}

pub(crate) fn is_file_write_tool(tool_name: &str) -> bool {
    lookup_tool(tool_name)
        .map(ToolSpec::is_file_write)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_tools_share_gate_and_kind() {
        let file_edit = lookup_tool("file_edit").unwrap();
        let text_replace = lookup_tool("text_replace").unwrap();
        let write_file = lookup_tool("write_file").unwrap();

        assert_eq!(file_edit.kind, "file_edit");
        assert_eq!(text_replace.kind, "file_edit");
        assert_eq!(write_file.kind, "file_edit");
        assert!(file_edit.is_file_write());
        assert!(text_replace.is_file_write());
        assert!(write_file.is_file_write());
    }

    #[test]
    fn channel_kind_is_known_without_builtin_route() {
        assert!(lookup_tool("feishu_reply_media").is_none());
        assert_eq!(tool_kind("feishu_reply_media"), "channel");
    }
}
