//! Build the `<available_mcp_servers>` context block.

use std::collections::BTreeSet;

use super::xml::{block, tag};

pub fn build_available_mcp_servers(server_names: &[String]) -> String {
    if server_names.is_empty() {
        return "<available_mcp_servers>\n</available_mcp_servers>".to_string();
    }

    // Python sorts case-insensitively after dedup. `BTreeSet` handles dedup;
    // we sort with `to_lowercase` comparison to mirror the key function.
    let mut unique: Vec<String> = server_names
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    unique.sort_by_key(|name| name.to_lowercase());

    let chunks: Vec<String> = unique
        .iter()
        .map(|name| block("server", &tag("name", name)))
        .collect();
    block("available_mcp_servers", &chunks.join("\n\n"))
}
