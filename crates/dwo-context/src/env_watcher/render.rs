use crate::prompt::{ChannelCapabilitySnapshot, SkillSnapshot, xml_escape};

use super::EnvChange;

pub(super) fn render(change: &EnvChange) -> String {
    if let EnvChange::Channels { previous, current } = change {
        return render_channels(previous, current);
    }
    let body = match change {
        EnvChange::AgentPrompt { content } => format!(
            "The agent system prompt changed. Use this replacement content from now on:\n\n{}",
            xml_escape(content)
        ),
        EnvChange::Rules { rules } => {
            let current = if rules.is_empty() {
                "(no AGENTS.md rules are currently present)".to_string()
            } else {
                rules
                    .iter()
                    .map(|rule| {
                        format!(
                            "source: {}\n{}",
                            xml_escape(&rule.path.display().to_string()),
                            xml_escape(&rule.content)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            format!(
                "The AGENTS.md rules changed. Replace the previous rules from the fixed profile and session-cwd sources with:\n\n{current}"
            )
        }
        EnvChange::Skills { skills } => format!(
            "The available skills changed. The current catalog is:\n{}",
            render_skills(skills)
        ),
        EnvChange::Channels { .. } => unreachable!("channel changes return above"),
        EnvChange::Mcp {
            config: Some(config),
        } => config.render(),
        EnvChange::Mcp { config: None } => {
            "<mcp state=\"removed\">\nNo MCP servers are currently configured.\n</mcp>".to_string()
        }
        EnvChange::Environment { environment } => format!(
            "The runtime environment changed:\ncwd: {}\nshell: {}\nplatform: {}\ncurrent_date: {}\ntimezone: {}",
            xml_escape(&environment.cwd),
            xml_escape(&environment.shell),
            xml_escape(&environment.platform),
            xml_escape(&environment.current_date),
            xml_escape(&environment.timezone)
        ),
    };
    format!("<env_watcher>\n{body}\n</env_watcher>")
}

fn render_channels(
    previous: &[ChannelCapabilitySnapshot],
    current: &[ChannelCapabilitySnapshot],
) -> String {
    let mut blocks = current
        .iter()
        .map(ChannelCapabilitySnapshot::render)
        .collect::<Vec<_>>();
    blocks.extend(
        previous
            .iter()
            .filter(|old| !current.iter().any(|new| new.name == old.name))
            .map(|channel| ChannelCapabilitySnapshot::render_removed(&channel.name)),
    );
    format!(
        "<channels>\nThe available channel adapters changed:\n{}\n</channels>",
        blocks.join("\n\n")
    )
}

fn render_skills(skills: &[SkillSnapshot]) -> String {
    if skills.is_empty() {
        return "(none)".to_string();
    }
    skills
        .iter()
        .map(|skill| {
            format!(
                "- {}: {} ({})",
                xml_escape(&skill.name),
                xml_escape(&skill.description),
                xml_escape(&skill.path.display().to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
