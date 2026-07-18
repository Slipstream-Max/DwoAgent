use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;
use weixin_agent::{LoginStatus, QrLoginSession, StandaloneQrLogin, WeixinConfig};

const WEIXIN_CHANNEL: &str = "weixin";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WeixinChannelConfig {
    pub enabled: bool,
    pub replay_turns: usize,
    pub markdown_filter: bool,
    #[serde(default = "default_true")]
    pub media_input: bool,
}

fn default_true() -> bool {
    true
}

impl WeixinChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.weixin.replayTurns must be at most 10");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChannelState {
    pub selected_session_id: Option<String>,
    pub sync_buf: Option<String>,
    pub last_event_seq: u64,
    pub context_tokens: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WeixinSecret {
    pub(crate) bot_token: String,
    pub(crate) base_url: String,
    pub(crate) ilink_bot_id: String,
    pub(crate) bound_user_id: String,
}

impl WeixinSecret {
    fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("botToken", self.bot_token.as_str()),
            ("baseUrl", self.base_url.as_str()),
            ("ilinkBotId", self.ilink_bot_id.as_str()),
            ("boundUserId", self.bound_user_id.as_str()),
        ] {
            if value.trim().is_empty() {
                bail!("channels.weixin secret field {field} must not be empty");
            }
        }
        Ok(())
    }
}

pub(crate) struct WeixinRuntime {
    pub(crate) config: WeixinChannelConfig,
    pub(crate) state: ChannelState,
    pub(crate) secret: WeixinSecret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSummary {
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
    pub selected_session_id: Option<String>,
    pub bound_user_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WeixinLoginStart {
    pub binding_id: String,
    pub qrcode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WeixinLoginProgress {
    Waiting,
    Scanned,
    Confirmed { channel: ChannelSummary },
    Expired,
    NeedVerifyCode,
    Failed { message: String },
}

struct PendingLogin {
    session: QrLoginSession,
    config: WeixinConfig,
}

pub struct ChannelManager {
    root: PathBuf,
    weixin: Option<WeixinChannelConfig>,
    pending: Mutex<HashMap<String, PendingLogin>>,
}

impl ChannelManager {
    pub async fn new(
        profile_root: &Path,
        channels: &BTreeMap<String, serde_yaml::Value>,
    ) -> Result<Self> {
        if let Some(unsupported) = channels.keys().find(|name| name.as_str() != WEIXIN_CHANNEL) {
            bail!("unsupported channel configuration: channels.{unsupported}");
        }
        let weixin = channels
            .get(WEIXIN_CHANNEL)
            .cloned()
            .map(serde_yaml::from_value::<WeixinChannelConfig>)
            .transpose()
            .context("parse channels.weixin")?;
        if let Some(config) = &weixin {
            config.validate()?;
        }
        let root = profile_root.join("channels").join(WEIXIN_CHANNEL);
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self {
            root,
            weixin,
            pending: Mutex::new(HashMap::new()),
        })
    }

    pub async fn list(&self) -> Result<Vec<ChannelSummary>> {
        let Some(config) = &self.weixin else {
            return Ok(Vec::new());
        };
        let state: ChannelState = read_yaml_or_default(&self.runtime_path()).await?;
        let (connected, bound_user_id) = if self.secret_path().is_file() {
            let secret = read_yaml::<WeixinSecret>(&self.secret_path()).await?;
            secret.validate()?;
            (true, Some(secret.bound_user_id))
        } else {
            (false, None)
        };
        Ok(vec![ChannelSummary {
            name: WEIXIN_CHANNEL.to_string(),
            enabled: config.enabled,
            connected,
            selected_session_id: state.selected_session_id,
            bound_user_id,
        }])
    }

    pub(crate) async fn bound_weixin_user(&self) -> Result<String> {
        Ok(self.load_weixin().await?.secret.bound_user_id)
    }

    pub async fn remove_weixin(&self) -> Result<bool> {
        let existed = self.secret_path().is_file() || self.runtime_path().is_file();
        for path in [self.secret_path(), self.runtime_path()] {
            if path.is_file() {
                tokio::fs::remove_file(path).await?;
            }
        }
        Ok(existed)
    }

    pub(crate) async fn load_weixin(&self) -> Result<WeixinRuntime> {
        let config = self
            .weixin
            .clone()
            .context("channels.weixin is not configured")?;
        let secret: WeixinSecret = read_yaml(&self.secret_path()).await?;
        secret.validate()?;
        Ok(WeixinRuntime {
            state: read_yaml_or_default(&self.runtime_path()).await?,
            secret,
            config,
        })
    }

    pub(crate) async fn save_state(&self, state: &ChannelState) -> Result<()> {
        let path = self.runtime_path();
        write_yaml(&path, state).await?;
        set_private_permissions(&path).await
    }

    pub async fn begin_weixin_login(&self) -> Result<WeixinLoginStart> {
        let settings = self
            .weixin
            .as_ref()
            .context("channels.weixin is not configured in profile.yaml")?;
        let config = WeixinConfig::builder()
            .token("")
            .markdown_filter(settings.markdown_filter)
            .build()?;
        let login = StandaloneQrLogin::new(&config);
        let existing = self.existing_weixin_tokens().await?;
        let session = login.start(None, &existing).await?;
        let binding_id = format!("binding-{}", Uuid::new_v4());
        let qrcode = session.qrcode_img_content.clone();
        self.pending
            .lock()
            .await
            .insert(binding_id.clone(), PendingLogin { session, config });
        Ok(WeixinLoginStart { binding_id, qrcode })
    }

    pub async fn poll_weixin_login(
        &self,
        binding_id: &str,
        verify_code: Option<&str>,
    ) -> Result<WeixinLoginProgress> {
        let mut pending = self.pending.lock().await;
        let login = pending
            .get(binding_id)
            .ok_or_else(|| anyhow::anyhow!("unknown or completed binding: {binding_id}"))?;
        let api = StandaloneQrLogin::new(&login.config);
        let status = api.poll_status(&login.session, verify_code).await?;
        match status {
            LoginStatus::Wait | LoginStatus::ScannedButRedirect { .. } => {
                Ok(WeixinLoginProgress::Waiting)
            }
            LoginStatus::Scanned => Ok(WeixinLoginProgress::Scanned),
            LoginStatus::Expired => {
                pending.remove(binding_id);
                Ok(WeixinLoginProgress::Expired)
            }
            LoginStatus::NeedVerifyCode => Ok(WeixinLoginProgress::NeedVerifyCode),
            LoginStatus::Confirmed {
                bot_token,
                ilink_bot_id,
                base_url,
                ilink_user_id,
            } => {
                pending.remove(binding_id);
                drop(pending);
                self.save_weixin(WeixinSecret {
                    bot_token,
                    base_url,
                    ilink_bot_id,
                    bound_user_id: ilink_user_id,
                })
                .await?;
                let channel = self
                    .list()
                    .await?
                    .into_iter()
                    .next()
                    .expect("configured Weixin channel is listed");
                Ok(WeixinLoginProgress::Confirmed { channel })
            }
            LoginStatus::VerifyCodeBlocked | LoginStatus::BindedRedirect => {
                pending.remove(binding_id);
                Ok(WeixinLoginProgress::Failed {
                    message: "Weixin rejected this QR login".to_string(),
                })
            }
            _ => Ok(WeixinLoginProgress::Waiting),
        }
    }

    async fn save_weixin(&self, secret: WeixinSecret) -> Result<()> {
        secret.validate()?;
        tokio::fs::create_dir_all(&self.root).await?;
        self.save_state(&ChannelState::default()).await?;
        let secret_path = self.secret_path();
        write_yaml(&secret_path, &secret).await?;
        set_private_permissions(&secret_path).await?;
        Ok(())
    }

    async fn existing_weixin_tokens(&self) -> Result<Vec<String>> {
        if !self.secret_path().is_file() {
            return Ok(Vec::new());
        }
        let secret: WeixinSecret = read_yaml(&self.secret_path()).await?;
        secret.validate()?;
        Ok(vec![secret.bot_token])
    }

    fn runtime_path(&self) -> PathBuf {
        self.root.join("runtime.yaml")
    }

    fn secret_path(&self) -> PathBuf {
        self.root.join("secret.yaml")
    }
}

async fn read_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let source = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    serde_yaml::from_str(&source).with_context(|| format!("parse {}", path.display()))
}

async fn read_yaml_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if !path.is_file() {
        return Ok(T::default());
    }
    read_yaml(path).await
}

async fn write_yaml(path: &Path, value: &impl Serialize) -> Result<()> {
    let source = serde_yaml::to_string(value)?;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    tokio::fs::write(&temporary, source).await?;
    if path.is_file() {
        tokio::fs::remove_file(path).await?;
    }
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

pub async fn wait_before_poll() {
    tokio::time::sleep(Duration::from_secs(2)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn profile_settings_are_validated_before_channel_state_is_loaded() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
markdownFilter: true
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(WEIXIN_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let summary = manager.list().await.unwrap();
        assert_eq!(summary.len(), 1);
        assert!(summary[0].enabled);
        assert!(!summary[0].connected);
        assert!(manager.weixin.as_ref().unwrap().media_input);
    }

    #[tokio::test]
    async fn unknown_weixin_setting_is_rejected() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
markdownFilter: true
flushIntervalMs: 10
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(WEIXIN_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let error = ChannelManager::new(root.path(), &channels)
            .await
            .err()
            .expect("unknown setting should fail");

        assert!(format!("{error:#}").contains("flushIntervalMs"));
    }

    #[tokio::test]
    async fn replay_turns_cannot_exceed_weixin_message_budget() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 11
markdownFilter: true
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(WEIXIN_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let error = ChannelManager::new(root.path(), &channels)
            .await
            .err()
            .expect("replay limit should fail validation");

        assert!(format!("{error:#}").contains("replayTurns must be at most 10"));
    }

    #[tokio::test]
    async fn runtime_state_is_persisted_as_yaml_with_context_tokens() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 3
markdownFilter: false
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(WEIXIN_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();
        let state = ChannelState {
            selected_session_id: Some("session-test".to_string()),
            sync_buf: Some("cursor".to_string()),
            context_tokens: HashMap::from([("user".to_string(), "token".to_string())]),
            ..ChannelState::default()
        };

        manager.save_state(&state).await.unwrap();

        let source = tokio::fs::read_to_string(manager.runtime_path())
            .await
            .unwrap();
        assert!(source.contains("syncBuf: cursor"));
        assert!(source.contains("contextTokens:"));
        assert!(!source.contains("streamMode"));
        assert!(!manager.root.join("state.json").exists());
    }
}
