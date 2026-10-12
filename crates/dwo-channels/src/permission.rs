use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dwo_agent_service::SessionId;
use tokio::sync::Mutex;

pub struct PermissionAction {
    pub group_id: String,
    pub session_id: SessionId,
    pub request_id: String,
    pub allowed: bool,
    pub expires_at: Instant,
}

pub type PermissionActionStore = Mutex<HashMap<String, PermissionAction>>;

pub fn new_action_id() -> Result<String> {
    let mut bytes = [0_u8; 9];
    getrandom::fill(&mut bytes).context("generate permission action id")?;
    Ok(format!("perm_{}", URL_SAFE_NO_PAD.encode(bytes)))
}

pub async fn take_action(
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

pub async fn remove_group(store: &PermissionActionStore, group_id: &str) {
    store
        .lock()
        .await
        .retain(|_, action| action.group_id != group_id);
}

/// 一张已发出的权限卡片（飞书专属）：点击解完权限后要靠它把卡片收口 ——
/// 去掉按钮、标上结果（「按过就不能再按」）。
pub struct PermissionCard {
    pub message_id: String,
    pub text: String,
    pub expires_at: Instant,
}

pub type PermissionCardStore = Mutex<HashMap<String, PermissionCard>>;

pub async fn put_card(store: &PermissionCardStore, group_id: &str, card: PermissionCard) {
    let mut store = store.lock().await;
    store.retain(|_, card| Instant::now() < card.expires_at);
    store.insert(group_id.to_string(), card);
}

pub async fn take_card(store: &PermissionCardStore, group_id: &str) -> Option<PermissionCard> {
    let mut store = store.lock().await;
    store.retain(|_, card| Instant::now() < card.expires_at);
    store.remove(group_id)
}
