//! Policy mode and permission decision helpers.

use anyhow::{Result, bail};

use super::constants::{
    MODE_ALLOW_ALL, MODE_BLOCK_ALL, MODE_CONFIRM, PERMISSION_ALLOW_ONCE, PERMISSION_CANCELLED,
    PERMISSION_REJECT_ONCE,
};

/// Outcome of `resolve_tool_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOutcome {
    pub allowed: bool,
    pub next_mode: String,
    pub mode_changed: bool,
}

pub fn resolve_tool_permission(mode_id: &str, decision: Option<&str>) -> Result<PermissionOutcome> {
    if mode_id == MODE_ALLOW_ALL {
        return Ok(PermissionOutcome {
            allowed: true,
            next_mode: mode_id.to_string(),
            mode_changed: false,
        });
    }
    if mode_id == MODE_BLOCK_ALL {
        return Ok(PermissionOutcome {
            allowed: false,
            next_mode: mode_id.to_string(),
            mode_changed: false,
        });
    }
    if mode_id != MODE_CONFIRM {
        bail!("Invalid mode_id: {mode_id}");
    }
    let decision =
        decision.ok_or_else(|| anyhow::anyhow!("decision is required when mode is confirm"))?;

    let allowed = match decision {
        PERMISSION_ALLOW_ONCE => true,
        PERMISSION_REJECT_ONCE | PERMISSION_CANCELLED => false,
        other => bail!("Invalid permission decision: {other}"),
    };
    Ok(PermissionOutcome {
        allowed,
        next_mode: MODE_CONFIRM.to_string(),
        mode_changed: false,
    })
}

pub fn clamp_subagent_policy(parent_mode: &str, requested_mode: Option<&str>) -> Result<String> {
    if parent_mode == MODE_BLOCK_ALL {
        return Ok(MODE_BLOCK_ALL.to_string());
    }

    if parent_mode == MODE_CONFIRM {
        return match requested_mode {
            Some(MODE_ALLOW_ALL) => Ok(MODE_CONFIRM.to_string()),
            None => Ok(MODE_CONFIRM.to_string()),
            Some(MODE_CONFIRM) => Ok(MODE_CONFIRM.to_string()),
            Some(MODE_BLOCK_ALL) => Ok(MODE_BLOCK_ALL.to_string()),
            Some(other) => bail!("Invalid requested subagent mode: {other}"),
        };
    }

    if parent_mode == MODE_ALLOW_ALL {
        return match requested_mode {
            None => Ok(MODE_ALLOW_ALL.to_string()),
            Some(MODE_ALLOW_ALL) => Ok(MODE_ALLOW_ALL.to_string()),
            Some(MODE_CONFIRM) => Ok(MODE_CONFIRM.to_string()),
            Some(MODE_BLOCK_ALL) => Ok(MODE_BLOCK_ALL.to_string()),
            Some(other) => bail!("Invalid requested subagent mode: {other}"),
        };
    }

    bail!("Invalid parent mode: {parent_mode}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_policy_is_clamped_at_creation_to_parent_mode() {
        assert_eq!(
            clamp_subagent_policy(MODE_CONFIRM, Some(MODE_ALLOW_ALL)).unwrap(),
            MODE_CONFIRM
        );
        assert_eq!(
            clamp_subagent_policy(MODE_CONFIRM, Some(MODE_BLOCK_ALL)).unwrap(),
            MODE_BLOCK_ALL
        );
        assert_eq!(
            clamp_subagent_policy(MODE_CONFIRM, None).unwrap(),
            MODE_CONFIRM
        );

        assert_eq!(
            clamp_subagent_policy(MODE_BLOCK_ALL, Some(MODE_ALLOW_ALL)).unwrap(),
            MODE_BLOCK_ALL
        );
        assert_eq!(
            clamp_subagent_policy(MODE_BLOCK_ALL, Some(MODE_CONFIRM)).unwrap(),
            MODE_BLOCK_ALL
        );

        assert_eq!(
            clamp_subagent_policy(MODE_ALLOW_ALL, Some(MODE_CONFIRM)).unwrap(),
            MODE_CONFIRM
        );
        assert_eq!(
            clamp_subagent_policy(MODE_ALLOW_ALL, Some(MODE_BLOCK_ALL)).unwrap(),
            MODE_BLOCK_ALL
        );
        assert_eq!(
            clamp_subagent_policy(MODE_ALLOW_ALL, None).unwrap(),
            MODE_ALLOW_ALL
        );
    }
}
