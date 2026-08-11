use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use dwo_context::{ContentBlock, MessageContent};

const SKILL_COMMAND: &str = "/skill";
const MCP_COMMAND: &str = "/mcp";

pub(super) const COMMAND_DESCRIPTIONS: [(&str, &str); 2] = [
    (
        "skill",
        "Request an available skill by name, optionally followed by a prompt.",
    ),
    (
        "mcp",
        "Request an available MCP server by name, optionally followed by a prompt.",
    ),
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirectiveKinds {
    pub(crate) skill: bool,
    pub(crate) mcp: bool,
}

impl DirectiveKinds {
    pub(crate) fn is_empty(&self) -> bool {
        !self.skill && !self.mcp
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AvailableSkill {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectiveKind {
    Skill,
    Mcp,
}

#[derive(Debug)]
struct ParsedDirective<'a> {
    kind: DirectiveKind,
    name: &'a str,
    end: usize,
}

pub(crate) fn routes_to_channel_command(text: &str) -> bool {
    text.starts_with('/') && !starts_with_prompt_directive(text)
}

fn starts_with_prompt_directive(text: &str) -> bool {
    let text = text.trim_start();
    [SKILL_COMMAND, MCP_COMMAND].into_iter().any(|command| {
        text.strip_prefix(command).is_some_and(|remainder| {
            remainder.is_empty() || remainder.chars().next().is_some_and(char::is_whitespace)
        })
    })
}

pub(crate) fn directive_kinds(content: &MessageContent) -> DirectiveKinds {
    let mut kinds = DirectiveKinds::default();
    for block in content.as_blocks() {
        let ContentBlock::Text { text, .. } = block else {
            continue;
        };
        for (start, _) in text.match_indices('/') {
            let Some(directive) = parse_at(text, start) else {
                continue;
            };
            match directive.kind {
                DirectiveKind::Skill => kinds.skill = true,
                DirectiveKind::Mcp => kinds.mcp = true,
            }
            if kinds.skill && kinds.mcp {
                return kinds;
            }
        }
    }
    kinds
}

pub(crate) fn expand(
    content: MessageContent,
    skills: &[AvailableSkill],
    mcp_servers: &[String],
) -> MessageContent {
    let skills = skills
        .iter()
        .map(|skill| (skill.name.as_str(), skill.path.as_path()))
        .collect::<HashMap<_, _>>();
    let mcp_servers = mcp_servers
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    MessageContent::blocks(
        content
            .into_blocks()
            .into_iter()
            .map(|block| match block {
                ContentBlock::Text {
                    text,
                    annotations,
                    meta,
                } => ContentBlock::Text {
                    text: expand_text(&text, &skills, &mcp_servers),
                    annotations,
                    meta,
                },
                other => other,
            })
            .collect(),
    )
}

fn expand_text(
    text: &str,
    skills: &HashMap<&str, &std::path::Path>,
    mcp_servers: &HashSet<&str>,
) -> String {
    let mut replacements = Vec::new();
    let mut covered_until = 0;
    for (start, _) in text.match_indices('/') {
        if start < covered_until {
            continue;
        }
        let Some(directive) = parse_at(text, start) else {
            continue;
        };
        let replacement = match directive.kind {
            DirectiveKind::Skill => skills
                .get(directive.name)
                .map(|path| render_skill_request(directive.name, path)),
            DirectiveKind::Mcp => mcp_servers
                .contains(directive.name)
                .then(|| render_mcp_request(directive.name)),
        };
        if let Some(replacement) = replacement {
            covered_until = directive.end;
            replacements.push((start, directive.end, replacement));
        }
    }
    if replacements.is_empty() {
        return text.to_string();
    }

    let extra = replacements
        .iter()
        .map(|(start, end, replacement)| replacement.len().saturating_sub(end - start))
        .sum::<usize>();
    let mut output = String::with_capacity(text.len().saturating_add(extra));
    let mut cursor = 0;
    for (start, end, replacement) in replacements {
        output.push_str(&text[cursor..start]);
        output.push_str(&replacement);
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    output
}

fn parse_at(text: &str, start: usize) -> Option<ParsedDirective<'_>> {
    let suffix = text.get(start..)?;
    let (kind, command) = if suffix.starts_with(SKILL_COMMAND) {
        (DirectiveKind::Skill, SKILL_COMMAND)
    } else if suffix.starts_with(MCP_COMMAND) {
        (DirectiveKind::Mcp, MCP_COMMAND)
    } else {
        return None;
    };
    let remainder = suffix.get(command.len()..)?;
    if !remainder.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let name_start = remainder.find(|character: char| !character.is_whitespace())?;
    let name_and_rest = &remainder[name_start..];
    let name_end = name_and_rest
        .find(char::is_whitespace)
        .unwrap_or(name_and_rest.len());
    let name = &name_and_rest[..name_end];
    (!name.is_empty()).then_some(ParsedDirective {
        kind,
        name,
        end: start + command.len() + name_start + name_end,
    })
}

fn render_skill_request(name: &str, path: &std::path::Path) -> String {
    let name = xml_escape(name);
    let path = xml_escape(&path.display().to_string());
    format!(
        "<skill_request name=\"{name}\" path=\"{path}\">\nThe user wants to use the {name} skill. Use the read_file tool to read the SKILL.md at {path} before acting, then follow its instructions.\n</skill_request>"
    )
}

fn render_mcp_request(name: &str) -> String {
    let name = xml_escape(name);
    format!(
        "<mcp_request name=\"{name}\">\nThe user wants to use the {name} MCP server. Use `dwo mcp search` in the terminal with {name} as the query to discover its tools, then use the matching MCP tool for the request.\n</mcp_request>"
    )
}

fn xml_escape(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use dwo_context::ContentAnnotations;

    use super::*;

    fn skills() -> Vec<AvailableSkill> {
        vec![
            AvailableSkill {
                name: "review".to_string(),
                path: PathBuf::from("C:/skills/review/SKILL.md"),
            },
            AvailableSkill {
                name: "x&y".to_string(),
                path: PathBuf::from("C:/skills/x&y/SKILL.md"),
            },
        ]
    }

    #[test]
    fn expands_matching_directives_anywhere_and_keeps_the_prompt() {
        let content = MessageContent::text(
            "请/skill review inspect this, then /mcp github open issue /skill review",
        );
        let expanded = expand(content, &skills(), &["github".to_string()]);
        let text = expanded.as_text().unwrap();
        assert_eq!(text.matches("<skill_request").count(), 2);
        assert_eq!(text.matches("<mcp_request").count(), 1);
        assert!(text.contains(" inspect this, then "));
        assert!(text.ends_with("</skill_request>"));
    }

    #[test]
    fn leaves_bare_unknown_and_similar_commands_unchanged() {
        for text in [
            "/skill",
            "/skill ",
            "/mcp",
            "/mcp missing prompt",
            "/skills review",
            "/mcpx github",
        ] {
            let content = MessageContent::text(text);
            assert_eq!(
                expand(content.clone(), &skills(), &["github".to_string()]),
                content
            );
        }
    }

    #[test]
    fn escapes_xml_and_preserves_structured_content_metadata() {
        let expected_annotations = ContentAnnotations {
            priority: Some(0.5),
            ..ContentAnnotations::default()
        };
        let content = MessageContent::blocks(vec![
            ContentBlock::Text {
                text: "/skill x&y".to_string(),
                annotations: Some(expected_annotations.clone()),
                meta: None,
            },
            ContentBlock::image("image/png", "data"),
        ]);
        let expanded = expand(content, &skills(), &[]);
        let ContentBlock::Text {
            text, annotations, ..
        } = &expanded.as_blocks()[0]
        else {
            panic!("expected text block")
        };
        assert!(text.contains("name=\"x&amp;y\""));
        assert!(text.contains("C:/skills/x&amp;y/SKILL.md"));
        assert_eq!(annotations.as_ref(), Some(&expected_annotations));
        assert!(matches!(
            expanded.as_blocks()[1],
            ContentBlock::Image { .. }
        ));
    }

    #[test]
    fn matched_directives_do_not_create_overlapping_replacements() {
        let expanded = expand(
            MessageContent::text("/skill /mcp github"),
            &[AvailableSkill {
                name: "/mcp".to_string(),
                path: PathBuf::from("C:/skills/mcp/SKILL.md"),
            }],
            &["github".to_string()],
        );
        let text = expanded.as_text().unwrap();
        assert_eq!(text.matches("<skill_request").count(), 1);
        assert_eq!(text.matches("<mcp_request").count(), 0);
        assert!(text.ends_with(" github"));
    }

    #[test]
    fn recognizes_only_exact_prompt_directive_prefixes() {
        assert!(starts_with_prompt_directive(" /skill"));
        assert!(starts_with_prompt_directive("/mcp github"));
        assert!(!starts_with_prompt_directive("/skills review"));
        assert!(!starts_with_prompt_directive("hello /skill review"));
        assert!(!routes_to_channel_command("/skill review"));
        assert!(!routes_to_channel_command("/mcp"));
        assert!(routes_to_channel_command("/skills review"));
        assert!(routes_to_channel_command("/status"));
    }
}
