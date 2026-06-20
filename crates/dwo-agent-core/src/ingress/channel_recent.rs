//! Recent transcript rendering for channel session switches.

use std::path::Path;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::agent::activity::event::{EVENT_AGENT_MESSAGE_CHUNK, EVENT_USER_MESSAGE_CHUNK};
use crate::agent::session::SESSION_CLIENT_TRANSCRIPT_FILE;
use crate::config::models::SessionTranscriptEvent;
use crate::utils::files::read_utf8_text;

const RECENT_CONTEXT_ENTRY_LIMIT: usize = 800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogueRole {
    User,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DialogueEntry {
    role: DialogueRole,
    text: String,
}

pub(super) fn render_recent_session_context(session_dir: &Path) -> Result<Option<String>> {
    let transcript_path = session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
    if !transcript_path.is_file() {
        return Ok(None);
    }
    let text = read_utf8_text(&transcript_path)?;
    Ok(render_recent_context_from_transcript(&text))
}

fn render_recent_context_from_transcript(text: &str) -> Option<String> {
    let entries = dialogue_entries_from_transcript(text);
    let selected = select_recent_dialogue_entries(&entries);
    if selected.is_empty() {
        return None;
    }
    Some(render_dialogue_entries(&selected))
}

fn dialogue_entries_from_transcript(text: &str) -> Vec<DialogueEntry> {
    let mut entries = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Ok(event) = serde_json::from_str::<SessionTranscriptEvent>(line) else {
            continue;
        };
        let update_type = event
            .update
            .get("session_update")
            .and_then(Value::as_str)
            .unwrap_or("");
        let role = match update_type {
            EVENT_USER_MESSAGE_CHUNK => DialogueRole::User,
            EVENT_AGENT_MESSAGE_CHUNK => DialogueRole::Agent,
            _ => continue,
        };
        let Some(content) = event.update.get("content") else {
            continue;
        };
        let Some(text) = content_to_dialogue_text(content) else {
            continue;
        };
        push_dialogue_entry(&mut entries, role, text);
    }
    entries
}

fn push_dialogue_entry(entries: &mut Vec<DialogueEntry>, role: DialogueRole, text: String) {
    if text.trim().is_empty() {
        return;
    }
    if let Some(last) = entries.last_mut()
        && last.role == role
    {
        match role {
            DialogueRole::Agent => last.text.push_str(&text),
            DialogueRole::User => {
                if !last.text.is_empty() {
                    last.text.push('\n');
                }
                last.text.push_str(text.trim());
            }
        }
        return;
    }

    let text = match role {
        DialogueRole::Agent => text,
        DialogueRole::User => text.trim().to_string(),
    };
    entries.push(DialogueEntry { role, text });
}

fn select_recent_dialogue_entries(entries: &[DialogueEntry]) -> Vec<&DialogueEntry> {
    let Some(last) = entries.last() else {
        return Vec::new();
    };
    let last_index = entries.len() - 1;
    match last.role {
        DialogueRole::Agent => {
            if let Some(user_index) = (0..last_index)
                .rev()
                .find(|index| entries[*index].role == DialogueRole::User)
            {
                entries[user_index..=last_index].iter().collect()
            } else {
                vec![last]
            }
        }
        DialogueRole::User => {
            let Some(agent_index) = (0..last_index)
                .rev()
                .find(|index| entries[*index].role == DialogueRole::Agent)
            else {
                return vec![last];
            };
            let Some(user_index) = (0..agent_index)
                .rev()
                .find(|index| entries[*index].role == DialogueRole::User)
            else {
                return vec![&entries[agent_index], last];
            };
            let mut selected: Vec<&DialogueEntry> =
                entries[user_index..=agent_index].iter().collect();
            selected.push(last);
            selected
        }
    }
}

fn render_dialogue_entries(entries: &[&DialogueEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            let label = match entry.role {
                DialogueRole::User => "用户",
                DialogueRole::Agent => "Agent",
            };
            format!(
                "{label}：{}",
                truncate_for_channel(&entry.text, RECENT_CONTEXT_ENTRY_LIMIT)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_to_dialogue_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => non_empty(text),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(content_to_dialogue_text)
                .collect::<Vec<_>>()
                .join("\n");
            non_empty(&text)
        }
        Value::Object(obj) => content_object_to_dialogue_text(obj),
        _ => None,
    }
}

fn content_object_to_dialogue_text(obj: &Map<String, Value>) -> Option<String> {
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" | "input_text" => obj
            .get("text")
            .or_else(|| obj.get("input_text"))
            .and_then(value_to_text),
        "resource_link" => obj
            .get("name")
            .or_else(|| obj.get("uri"))
            .and_then(value_to_text)
            .map(|name| format!("[resource: {name}]")),
        "image_url" | "image" | "input_image" => Some("[image]".to_string()),
        _ => None,
    }
}

fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty(text),
        Value::Null => None,
        other => non_empty(&other.to_string()),
    }
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| text.to_string())
}

fn truncate_for_channel(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recent_context_selects_latest_user_and_agent_answer() {
        let text = transcript(&[
            (EVENT_USER_MESSAGE_CHUNK, "user1"),
            (EVENT_AGENT_MESSAGE_CHUNK, "response1"),
            (EVENT_USER_MESSAGE_CHUNK, "user2"),
            (EVENT_AGENT_MESSAGE_CHUNK, "response2"),
        ]);

        let rendered = render_recent_context_from_transcript(&text).unwrap();

        assert_eq!(rendered, "用户：user2\nAgent：response2");
    }

    #[test]
    fn recent_context_includes_previous_answer_when_latest_user_is_unanswered() {
        let text = transcript(&[
            (EVENT_USER_MESSAGE_CHUNK, "user1"),
            (EVENT_AGENT_MESSAGE_CHUNK, "response1"),
            (EVENT_USER_MESSAGE_CHUNK, "user2"),
        ]);

        let rendered = render_recent_context_from_transcript(&text).unwrap();

        assert_eq!(rendered, "用户：user1\nAgent：response1\n用户：user2");
    }

    #[test]
    fn recent_context_handles_single_user_only() {
        let text = transcript(&[(EVENT_USER_MESSAGE_CHUNK, "user1")]);

        let rendered = render_recent_context_from_transcript(&text).unwrap();

        assert_eq!(rendered, "用户：user1");
    }

    #[test]
    fn recent_context_merges_agent_chunks() {
        let text = transcript(&[
            (EVENT_USER_MESSAGE_CHUNK, "user1"),
            (EVENT_AGENT_MESSAGE_CHUNK, "res"),
            (EVENT_AGENT_MESSAGE_CHUNK, "ponse1"),
        ]);

        let rendered = render_recent_context_from_transcript(&text).unwrap();

        assert_eq!(rendered, "用户：user1\nAgent：response1");
    }

    fn transcript(events: &[(&str, &str)]) -> String {
        events
            .iter()
            .map(|(update_type, text)| {
                json!({
                    "updated_at": "2026-06-12T00:00:00.000000+00:00",
                    "update": {
                        "session_update": update_type,
                        "content": {"type": "text", "text": text}
                    }
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
