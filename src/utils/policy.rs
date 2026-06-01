//! Policy mode parsing and tool output helpers.

use anyhow::{Result, bail};
use serde_json::{Value, json};

const MODE_ALLOW_ALL: &str = "allow_all";
const MODE_BLOCK_ALL: &str = "block_all";
const MODE_CONFIRM: &str = "confirm";

pub fn parse_policy_mode(value: &str) -> Result<String> {
    let mode = value.trim();
    if mode.is_empty() {
        bail!("policy_mode cannot be empty");
    }
    if !matches!(mode, MODE_ALLOW_ALL | MODE_BLOCK_ALL | MODE_CONFIRM) {
        bail!("policy_mode must use one internal name: allow_all, block_all, confirm");
    }
    Ok(mode.to_string())
}

pub fn cancelled_tool_output() -> Value {
    json!({
        "status": "cancelled_by_user_interrupt",
        "message": "Tool call cancelled because user interrupt.",
    })
}
