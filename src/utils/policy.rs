//! Policy mode parsing and tool output helpers.

use anyhow::{Result, bail};
use serde_json::{Value, json};

const MODE_FULL_ACCESS: &str = "full_access";
const MODE_CONFIRM: &str = "confirm";
const MODE_WATCH: &str = "watch";

pub fn parse_policy_mode(value: &str) -> Result<String> {
    let mode = value.trim();
    if mode.is_empty() {
        bail!("policy_mode cannot be empty");
    }
    let normalized = match mode {
        MODE_FULL_ACCESS => MODE_FULL_ACCESS,
        MODE_CONFIRM => MODE_CONFIRM,
        MODE_WATCH => MODE_WATCH,
        _ => {
            bail!("policy_mode must use one internal name: full_access, confirm, watch");
        }
    };
    Ok(normalized.to_string())
}

pub fn policy_mode_rank(mode: &str) -> Result<u8> {
    match parse_policy_mode(mode)?.as_str() {
        MODE_WATCH => Ok(0),
        MODE_CONFIRM => Ok(1),
        MODE_FULL_ACCESS => Ok(2),
        other => bail!("invalid policy mode: {other}"),
    }
}

pub fn cancelled_tool_output() -> Value {
    json!({
        "status": "cancelled_by_user_interrupt",
        "message": "Tool call cancelled because user interrupt.",
    })
}
