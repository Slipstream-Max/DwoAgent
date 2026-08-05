use serde::Serialize;
use serde_json::Value;

use crate::{ContentBlock, ContextMessage, EmbeddedResourceContents, MessageContent};

const ASCII_UNITS_PER_TOKEN: u64 = 4;
const NON_ASCII_UNITS: u64 = ASCII_UNITS_PER_TOKEN;
const IMAGE_TOKENS: u64 = 1_200;
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const CONTENT_BLOCK_OVERHEAD_TOKENS: u64 = 2;
const OMITTED_MARKER: &str = "\n... content omitted ...\n";

pub fn estimate_text_tokens(text: &str) -> u64 {
    units_to_tokens(text_units(text))
}

pub fn estimate_content_tokens(content: &MessageContent) -> u64 {
    content
        .as_blocks()
        .iter()
        .map(estimate_block_tokens)
        .fold(0, u64::saturating_add)
}

pub fn estimate_message_tokens(message: &ContextMessage) -> u64 {
    if let Some(item) = &message.response_item {
        return MESSAGE_OVERHEAD_TOKENS.saturating_add(estimate_serialized_tokens(item));
    }
    let mut tokens =
        MESSAGE_OVERHEAD_TOKENS.saturating_add(estimate_content_tokens(&message.content));
    if let Some(reasoning) = &message.reasoning {
        tokens = tokens.saturating_add(estimate_text_tokens(reasoning));
    }
    for call in &message.tool_calls {
        tokens = tokens.saturating_add(estimate_serialized_tokens(call));
    }
    if let Some(id) = &message.tool_call_id {
        tokens = tokens.saturating_add(estimate_text_tokens(id));
    }
    if let Some(name) = &message.tool_name {
        tokens = tokens.saturating_add(estimate_text_tokens(name));
    }
    tokens
}

pub fn estimate_context_tokens(messages: &[ContextMessage], tools: &[Value]) -> u64 {
    let message_tokens = messages
        .iter()
        .map(estimate_message_tokens)
        .fold(0, u64::saturating_add);
    message_tokens.saturating_add(estimate_tool_tokens(tools))
}

pub fn estimate_tool_tokens(tools: &[Value]) -> u64 {
    estimate_serialized_tokens(tools)
}

pub(crate) fn cap_content_tokens(content: &MessageContent, budget: u64) -> MessageContent {
    if estimate_content_tokens(content) <= budget {
        return content.clone();
    }

    let mut remaining = budget;
    let mut blocks = Vec::new();
    for mut block in content.clone().into_blocks() {
        if remaining <= CONTENT_BLOCK_OVERHEAD_TOKENS {
            break;
        }
        let block_tokens = estimate_block_tokens(&block);
        if block_tokens <= remaining {
            remaining -= block_tokens;
            blocks.push(block);
            continue;
        }

        let ContentBlock::Text { text, .. } = &mut block else {
            break;
        };
        let text_budget = remaining.saturating_sub(CONTENT_BLOCK_OVERHEAD_TOKENS);
        *text = cap_text_tokens(text, text_budget);
        if !text.is_empty() {
            blocks.push(block);
        }
        break;
    }
    MessageContent::blocks(blocks)
}

fn estimate_block_tokens(block: &ContentBlock) -> u64 {
    let content = match block {
        ContentBlock::Text { text, .. } => estimate_text_tokens(text),
        ContentBlock::Image { .. } => IMAGE_TOKENS,
        ContentBlock::Audio { data, .. } => estimate_text_tokens(data),
        ContentBlock::Resource { resource, .. } => match resource {
            EmbeddedResourceContents::Text { uri, text, .. } => {
                estimate_text_tokens(uri).saturating_add(estimate_text_tokens(text))
            }
            EmbeddedResourceContents::Blob { uri, blob, .. } => {
                estimate_text_tokens(uri).saturating_add(estimate_text_tokens(blob))
            }
        },
        ContentBlock::ResourceLink {
            uri,
            name,
            title,
            description,
            ..
        } => {
            let mut tokens = estimate_text_tokens(uri).saturating_add(estimate_text_tokens(name));
            if let Some(title) = title {
                tokens = tokens.saturating_add(estimate_text_tokens(title));
            }
            if let Some(description) = description {
                tokens = tokens.saturating_add(estimate_text_tokens(description));
            }
            tokens
        }
    };
    CONTENT_BLOCK_OVERHEAD_TOKENS.saturating_add(content)
}

fn estimate_serialized_tokens<T: Serialize + ?Sized>(value: &T) -> u64 {
    serde_json::to_string(value)
        .map(|encoded| estimate_text_tokens(&encoded))
        .unwrap_or_default()
}

fn cap_text_tokens(value: &str, budget: u64) -> String {
    if estimate_text_tokens(value) <= budget {
        return value.to_string();
    }
    if budget == 0 {
        return String::new();
    }

    let budget_units = budget.saturating_mul(ASCII_UNITS_PER_TOKEN);
    let marker_units = text_units(OMITTED_MARKER);
    if budget_units <= marker_units {
        return take_prefix(value, budget_units);
    }

    let content_units = budget_units - marker_units;
    let head_units = content_units / 2;
    let tail_units = content_units - head_units;
    let head = take_prefix(value, head_units);
    let tail = take_suffix(value, tail_units, head.len());
    format!("{head}{OMITTED_MARKER}{tail}")
}

fn take_prefix(value: &str, budget_units: u64) -> String {
    let mut used = 0_u64;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let units = char_units(character);
        if used.saturating_add(units) > budget_units {
            break;
        }
        used += units;
        end = index + character.len_utf8();
    }
    value[..end].to_string()
}

fn take_suffix(value: &str, budget_units: u64, minimum_start: usize) -> String {
    let mut used = 0_u64;
    let mut start = value.len();
    for (index, character) in value.char_indices().rev() {
        if index < minimum_start {
            break;
        }
        let units = char_units(character);
        if used.saturating_add(units) > budget_units {
            break;
        }
        used += units;
        start = index;
    }
    value[start..].to_string()
}

fn text_units(text: &str) -> u64 {
    text.chars().map(char_units).fold(0, u64::saturating_add)
}

fn char_units(character: char) -> u64 {
    if character.is_ascii() {
        1
    } else {
        NON_ASCII_UNITS
    }
}

fn units_to_tokens(units: u64) -> u64 {
    units.saturating_add(ASCII_UNITS_PER_TOKEN - 1) / ASCII_UNITS_PER_TOKEN
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{ContextMessage, MessageContent};

    #[test]
    fn estimates_ascii_and_non_ascii_text_in_tokens() {
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert_eq!(estimate_text_tokens("abcde"), 2);
        assert_eq!(estimate_text_tokens("你好"), 2);
    }

    #[test]
    fn context_estimate_includes_reasoning_calls_results_and_schemas() {
        let messages = vec![ContextMessage::assistant_with_reasoning(
            "answer",
            Some("reasoning".to_string()),
            vec![json!({"id":"call-1","name":"terminal","arguments":{"command":"echo hi"}})],
        )];
        let without_tools = estimate_context_tokens(&messages, &[]);
        let with_tools = estimate_context_tokens(
            &messages,
            &[json!({"type":"function","function":{"name":"terminal"}})],
        );

        assert!(without_tools > estimate_content_tokens(&MessageContent::text("answer")));
        assert!(with_tools > without_tools);
    }

    #[test]
    fn native_response_item_is_counted_without_projected_content() {
        let item = json!({
            "type":"message",
            "role":"assistant",
            "content":[{"type":"output_text","text":"answer"}]
        });
        let message = ContextMessage::response_item(item.clone(), None);
        assert_eq!(
            estimate_message_tokens(&message),
            MESSAGE_OVERHEAD_TOKENS + estimate_serialized_tokens(&item)
        );
    }

    #[test]
    fn capped_text_preserves_its_beginning_and_end() {
        let content = MessageContent::text(format!("HEAD{}TAIL", "你".repeat(100)));
        let capped = cap_content_tokens(&content, 30);

        assert!(capped.contains("HEAD"));
        assert!(capped.contains("TAIL"));
        assert!(capped.contains("content omitted"));
        assert!(estimate_content_tokens(&capped) <= 30);
    }
}
