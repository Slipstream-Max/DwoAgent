mod builder;
mod channel;
mod environment;
mod mcp;
mod skills;

pub use builder::{
    AgentProfilePaths, ExternalRuleFile, PromptBuildError, PromptSnapshot, RuleSnapshot,
    SystemPromptBlock, SystemPromptBuilder,
};
pub use channel::ChannelCapabilitySnapshot;
pub use environment::EnvironmentSnapshot;
pub use mcp::McpSnapshot;
pub use skills::SkillSnapshot;

pub(crate) fn xml_block(name: &str, content: &str) -> String {
    let content = content.trim();
    if content.is_empty() {
        format!("<{name}>\n</{name}>")
    } else {
        format!("<{name}>\n{}\n</{name}>", xml_escape(content))
    }
}

pub(crate) fn xml_escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#x27;"),
            other => output.push(other),
        }
    }
    output
}

pub(crate) fn stable_fingerprint(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}
