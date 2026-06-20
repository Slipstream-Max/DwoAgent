//! XML helpers for agent context blocks.

/// HTML-escape the five core characters.
pub fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

/// Return an escaped one-value XML tag.
pub fn tag(name: &str, value: &str) -> String {
    format!("<{name}>\n{}\n</{name}>", html_escape(value))
}

/// Return a raw XML block; the body is emitted verbatim.
pub fn block(name: &str, content: &str) -> String {
    let text = content.trim();
    if text.is_empty() {
        format!("<{name}>\n</{name}>")
    } else {
        format!("<{name}>\n{text}\n</{name}>")
    }
}

/// Return an XML block containing HTML-escaped text content.
pub fn text_block(name: &str, content: &str) -> String {
    block(name, &html_escape(content.trim()))
}

/// Join context blocks into the root `<agent_context>` block.
pub fn join_agent_context(blocks: &[String]) -> String {
    let body: Vec<&str> = blocks
        .iter()
        .map(|b| b.trim())
        .filter(|b| !b.is_empty())
        .collect();
    if body.is_empty() {
        "<agent_context>\n</agent_context>".to_string()
    } else {
        format!("<agent_context>\n{}\n</agent_context>", body.join("\n\n"))
    }
}
