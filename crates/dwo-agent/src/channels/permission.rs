use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dwo_agent_service::SessionId;
use tokio::sync::Mutex;

pub(crate) struct PermissionAction {
    pub(crate) group_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) request_id: String,
    pub(crate) allowed: bool,
    pub(crate) expires_at: Instant,
}

pub(crate) type PermissionActionStore = Mutex<HashMap<String, PermissionAction>>;

pub(crate) fn new_action_id() -> Result<String> {
    let mut bytes = [0_u8; 9];
    getrandom::fill(&mut bytes).context("generate permission action id")?;
    Ok(format!("perm_{}", URL_SAFE_NO_PAD.encode(bytes)))
}

pub(crate) async fn take_action(
    store: &PermissionActionStore,
    action_id: &str,
) -> Option<PermissionAction> {
    let mut store = store.lock().await;
    store.retain(|_, action| Instant::now() < action.expires_at);
    let action = store.remove(action_id);
    if let Some(action) = &action {
        store.retain(|_, pending| pending.group_id != action.group_id);
    }
    action
}

pub(crate) async fn remove_group(store: &PermissionActionStore, group_id: &str) {
    store
        .lock()
        .await
        .retain(|_, action| action.group_id != group_id);
}
