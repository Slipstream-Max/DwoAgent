pub use crate::utils::policy::{cancelled_tool_output, parse_policy_mode};

pub const STATE_IDLE: &str = "idle";
pub const STATE_RUNNING: &str = "running";
pub const STATE_WAITING_USER_CONFIRM: &str = "waiting_user_confirm";
pub const STATE_CANCELLING: &str = "cancelling";
pub const STATE_STOP: &str = "stop";

pub const STOP_COMPLETED: &str = "completed";
pub const STOP_CANCELLED: &str = "cancelled";
pub const STOP_MAX_TURNS: &str = "max_turns";

pub const MODE_ALLOW_ALL: &str = "allow_all";
pub const MODE_BLOCK_ALL: &str = "block_all";
pub const MODE_CONFIRM: &str = "confirm";

pub const PERMISSION_ALLOW_ONCE: &str = "allow_once";
pub const PERMISSION_REJECT_ONCE: &str = "reject_once";
pub const PERMISSION_CANCELLED: &str = "cancelled";
