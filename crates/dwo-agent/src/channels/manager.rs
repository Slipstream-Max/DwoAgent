use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::UpdateKind;
use tokio::sync::Mutex;
use uuid::Uuid;
use weixin_agent::{LoginStatus, QrLoginSession, StandaloneQrLogin, WeixinConfig};

const WEIXIN_CHANNEL: &str = "weixin";
const TELEGRAM_CHANNEL: &str = "telegram";

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
pub struct WeixinChannelState {
    pub selected_session_id: Option<String>,
    pub sync_buf: Option<String>,
    pub last_event_seq: u64,
    pub context_tokens: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TelegramChannelConfig {
    pub enabled: bool,
    pub replay_turns: usize,
    pub bot_token_env: String,
    #[serde(default)]
    pub tg_proxy: Option<String>,
    #[serde(default = "default_true")]
    pub media_input: bool,
}

impl TelegramChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.telegram.replayTurns must be at most 10");
        }
        if self.bot_token_env.trim().is_empty() {
            bail!("channels.telegram.botTokenEnv must not be empty");
        }
        if self
            .tg_proxy
            .as_deref()
            .is_some_and(|proxy| proxy.trim().is_empty())
        {
            bail!("channels.telegram.tgProxy must be null or a non-empty HTTP proxy URL");
        }
        if let Some(proxy) = self.tg_proxy.as_deref()
            && !proxy.starts_with("http://")
            && !proxy.starts_with("https://")
        {
            bail!("channels.telegram.tgProxy must use an http:// or https:// URL");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TelegramChannelState {
    pub selected_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TelegramSecret {
    pub(crate) bot_id: u64,
    pub(crate) bot_username: String,
    pub(crate) bound_user_id: u64,
    pub(crate) bound_chat_id: i64,
}

impl TelegramSecret {
    fn validate(&self) -> Result<()> {
        if self.bot_id == 0 {
            bail!("channels.telegram secret field botId must be positive");
        }
        if self.bot_username.trim().is_empty() {
            bail!("channels.telegram secret field botUsername must not be empty");
        }
        if self.bound_user_id == 0 {
            bail!("channels.telegram secret field boundUserId must be positive");
        }
        if self.bound_chat_id <= 0 {
            bail!("channels.telegram secret field boundChatId must be a private chat id");
        }
        Ok(())
    }
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
    pub(crate) state: WeixinChannelState,
    pub(crate) secret: WeixinSecret,
}

pub(crate) struct TelegramRuntime {
    pub(crate) config: TelegramChannelConfig,
    pub(crate) state: TelegramChannelState,
    pub(crate) secret: TelegramSecret,
    pub(crate) bot_token: String,
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

#[derive(Debug, Clone, Serialize)]
pub struct TelegramBindStart {
    pub binding_id: String,
    pub code: String,
    pub bot_username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TelegramBindProgress {
    Waiting,
    Confirmed { channel: ChannelSummary },
    Expired,
}

struct PendingLogin {
    session: QrLoginSession,
    config: WeixinConfig,
}

struct PendingTelegramBind {
    bot: Bot,
    bot_id: u64,
    bot_username: String,
    code: String,
    offset: i32,
    expires_at: Instant,
}

pub struct ChannelManager {
    root: PathBuf,
    capability_root: PathBuf,
    weixin: Option<WeixinChannelConfig>,
    telegram: Option<TelegramChannelConfig>,
    pending_weixin: Mutex<HashMap<String, PendingLogin>>,
    pending_telegram: Mutex<HashMap<String, PendingTelegramBind>>,
}

impl ChannelManager {
    pub async fn new(
        profile_root: &Path,
        channels: &BTreeMap<String, serde_yaml::Value>,
    ) -> Result<Self> {
        if let Some(unsupported) = channels
            .keys()
            .find(|name| !matches!(name.as_str(), WEIXIN_CHANNEL | TELEGRAM_CHANNEL))
        {
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
        let telegram = channels
            .get(TELEGRAM_CHANNEL)
            .cloned()
            .map(serde_yaml::from_value::<TelegramChannelConfig>)
            .transpose()
            .context("parse channels.telegram")?;
        if let Some(config) = &telegram {
            config.validate()?;
        }
        let root = profile_root.join("channels");
        let capability_root = profile_root.join("runtime/channel-capabilities");
        tokio::fs::create_dir_all(&root).await?;
        tokio::fs::create_dir_all(&capability_root).await?;
        let manager = Self {
            root,
            capability_root,
            weixin,
            telegram,
            pending_weixin: Mutex::new(HashMap::new()),
            pending_telegram: Mutex::new(HashMap::new()),
        };
        manager.sync_capabilities().await?;
        Ok(manager)
    }

    pub async fn list(&self) -> Result<Vec<ChannelSummary>> {
        let mut summaries = Vec::new();
        if let Some(config) = &self.weixin {
            let state: WeixinChannelState =
                read_yaml_or_default(&self.weixin_runtime_path()).await?;
            let (connected, bound_user_id) = if self.weixin_secret_path().is_file() {
                let secret = read_yaml::<WeixinSecret>(&self.weixin_secret_path()).await?;
                secret.validate()?;
                (true, Some(secret.bound_user_id))
            } else {
                (false, None)
            };
            summaries.push(ChannelSummary {
                name: WEIXIN_CHANNEL.to_string(),
                enabled: config.enabled,
                connected,
                selected_session_id: state.selected_session_id,
                bound_user_id,
            });
        }
        if let Some(config) = &self.telegram {
            let state: TelegramChannelState =
                read_yaml_or_default(&self.telegram_runtime_path()).await?;
            let (connected, bound_user_id) = if self.telegram_secret_path().is_file() {
                let secret = read_yaml::<TelegramSecret>(&self.telegram_secret_path()).await?;
                secret.validate()?;
                (
                    resolve_env(&config.bot_token_env).is_ok(),
                    Some(secret.bound_user_id.to_string()),
                )
            } else {
                (false, None)
            };
            summaries.push(ChannelSummary {
                name: TELEGRAM_CHANNEL.to_string(),
                enabled: config.enabled,
                connected,
                selected_session_id: state.selected_session_id,
                bound_user_id,
            });
        }
        Ok(summaries)
    }

    pub(crate) async fn bound_weixin_user(&self) -> Result<String> {
        Ok(self.load_weixin().await?.secret.bound_user_id)
    }

    pub async fn remove_weixin(&self) -> Result<bool> {
        let existed = self.weixin_secret_path().is_file() || self.weixin_runtime_path().is_file();
        for path in [self.weixin_secret_path(), self.weixin_runtime_path()] {
            if path.is_file() {
                tokio::fs::remove_file(path).await?;
            }
        }
        self.sync_weixin_capability().await?;
        Ok(existed)
    }

    pub(crate) async fn load_weixin(&self) -> Result<WeixinRuntime> {
        let config = self
            .weixin
            .clone()
            .context("channels.weixin is not configured")?;
        let secret: WeixinSecret = read_yaml(&self.weixin_secret_path()).await?;
        secret.validate()?;
        Ok(WeixinRuntime {
            state: read_yaml_or_default(&self.weixin_runtime_path()).await?,
            secret,
            config,
        })
    }

    pub(crate) async fn save_weixin_state(&self, state: &WeixinChannelState) -> Result<()> {
        let path = self.weixin_runtime_path();
        tokio::fs::create_dir_all(self.weixin_root()).await?;
        write_yaml(&path, state).await?;
        set_private_permissions(&path).await
    }

    pub(crate) async fn load_telegram(&self) -> Result<TelegramRuntime> {
        let config = self
            .telegram
            .clone()
            .context("channels.telegram is not configured")?;
        let bot_token = resolve_env(&config.bot_token_env)
            .with_context(|| format!("resolve channels.telegram.{}", config.bot_token_env))?;
        let secret: TelegramSecret = read_yaml(&self.telegram_secret_path()).await?;
        secret.validate()?;
        Ok(TelegramRuntime {
            state: read_yaml_or_default(&self.telegram_runtime_path()).await?,
            config,
            secret,
            bot_token,
        })
    }

    pub(crate) async fn bound_telegram_chat(&self) -> Result<i64> {
        Ok(self.load_telegram().await?.secret.bound_chat_id)
    }

    pub async fn remove_telegram(&self) -> Result<bool> {
        let existed =
            self.telegram_secret_path().is_file() || self.telegram_runtime_path().is_file();
        for path in [self.telegram_secret_path(), self.telegram_runtime_path()] {
            if path.is_file() {
                tokio::fs::remove_file(path).await?;
            }
        }
        self.sync_telegram_capability().await?;
        Ok(existed)
    }

    pub(crate) async fn save_telegram_state(&self, state: &TelegramChannelState) -> Result<()> {
        let path = self.telegram_runtime_path();
        tokio::fs::create_dir_all(self.telegram_root()).await?;
        write_yaml(&path, state).await?;
        set_private_permissions(&path).await
    }

    pub async fn begin_telegram_bind(&self) -> Result<TelegramBindStart> {
        let config = self
            .telegram
            .as_ref()
            .context("channels.telegram is not configured in profile.yaml")?;
        let token = resolve_env(&config.bot_token_env)
            .with_context(|| format!("resolve channels.telegram.{}", config.bot_token_env))?;
        let bot = telegram_bot(&token, config.tg_proxy.as_deref())?;
        let me = bot.get_me().await?;
        let binding_id = format!("binding-{}", Uuid::new_v4());
        let code = Uuid::new_v4().simple().to_string()[..8].to_ascii_uppercase();
        let bot_username = me.username().to_string();
        let mut pending = self.pending_telegram.lock().await;
        pending.clear();
        pending.insert(
            binding_id.clone(),
            PendingTelegramBind {
                bot,
                bot_id: me.id.0,
                bot_username: bot_username.clone(),
                code: code.clone(),
                offset: 0,
                expires_at: Instant::now() + Duration::from_secs(10 * 60),
            },
        );
        Ok(TelegramBindStart {
            binding_id,
            code,
            bot_username,
        })
    }

    pub async fn poll_telegram_bind(&self, binding_id: &str) -> Result<TelegramBindProgress> {
        let (bot, code, offset, bot_id, bot_username) = {
            let mut pending = self.pending_telegram.lock().await;
            let binding = pending
                .get(binding_id)
                .ok_or_else(|| anyhow::anyhow!("unknown or completed binding: {binding_id}"))?;
            if Instant::now() >= binding.expires_at {
                pending.remove(binding_id);
                return Ok(TelegramBindProgress::Expired);
            }
            (
                binding.bot.clone(),
                binding.code.clone(),
                binding.offset,
                binding.bot_id,
                binding.bot_username.clone(),
            )
        };

        let updates = bot
            .get_updates()
            .offset(offset)
            .limit(100)
            .timeout(0)
            .await?;
        let next_offset = updates
            .last()
            .and_then(|update| i32::try_from(update.id.0).ok())
            .map_or(offset, |id| id.saturating_add(1));
        let matched = updates
            .iter()
            .find_map(|update| telegram_bind_identity(update, &code));

        self.pending_telegram
            .lock()
            .await
            .get_mut(binding_id)
            .ok_or_else(|| anyhow::anyhow!("unknown or completed binding: {binding_id}"))?
            .offset = next_offset;
        let Some((bound_user_id, bound_chat_id)) = matched else {
            return Ok(TelegramBindProgress::Waiting);
        };

        // Confirm the matching update before the long-polling runtime takes over.
        bot.get_updates()
            .offset(next_offset)
            .limit(1)
            .timeout(0)
            .await?;
        self.save_telegram(TelegramSecret {
            bot_id,
            bot_username,
            bound_user_id,
            bound_chat_id,
        })
        .await?;
        self.pending_telegram.lock().await.remove(binding_id);
        bot.send_message(
            ChatId(bound_chat_id),
            "Telegram channel bound to this private chat",
        )
        .await?;
        let channel = self
            .list()
            .await?
            .into_iter()
            .find(|channel| channel.name == TELEGRAM_CHANNEL)
            .expect("configured Telegram channel is listed");
        Ok(TelegramBindProgress::Confirmed { channel })
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
        self.pending_weixin
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
        let mut pending = self.pending_weixin.lock().await;
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
                    .find(|channel| channel.name == WEIXIN_CHANNEL)
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
        tokio::fs::create_dir_all(self.weixin_root()).await?;
        self.save_weixin_state(&WeixinChannelState::default())
            .await?;
        let secret_path = self.weixin_secret_path();
        write_yaml(&secret_path, &secret).await?;
        set_private_permissions(&secret_path).await?;
        self.sync_weixin_capability().await
    }

    async fn save_telegram(&self, secret: TelegramSecret) -> Result<()> {
        secret.validate()?;
        tokio::fs::create_dir_all(self.telegram_root()).await?;
        self.save_telegram_state(&TelegramChannelState::default())
            .await?;
        let secret_path = self.telegram_secret_path();
        write_yaml(&secret_path, &secret).await?;
        set_private_permissions(&secret_path).await?;
        self.sync_telegram_capability().await
    }

    async fn existing_weixin_tokens(&self) -> Result<Vec<String>> {
        if !self.weixin_secret_path().is_file() {
            return Ok(Vec::new());
        }
        let secret: WeixinSecret = read_yaml(&self.weixin_secret_path()).await?;
        secret.validate()?;
        Ok(vec![secret.bot_token])
    }

    async fn sync_capabilities(&self) -> Result<()> {
        self.sync_weixin_capability().await?;
        self.sync_telegram_capability().await
    }

    async fn sync_weixin_capability(&self) -> Result<()> {
        let available = if self.weixin.as_ref().is_some_and(|config| config.enabled) {
            read_yaml::<WeixinSecret>(&self.weixin_secret_path())
                .await
                .is_ok_and(|secret| secret.validate().is_ok())
        } else {
            false
        };
        self.sync_capability(WEIXIN_CHANNEL, available, super::weixin::CAPABILITY_PROMPT)
            .await
    }

    async fn sync_telegram_capability(&self) -> Result<()> {
        let available = if self.telegram.as_ref().is_some_and(|config| config.enabled) {
            read_yaml::<TelegramSecret>(&self.telegram_secret_path())
                .await
                .is_ok_and(|secret| secret.validate().is_ok())
        } else {
            false
        };
        self.sync_capability(
            TELEGRAM_CHANNEL,
            available,
            super::telegram::CAPABILITY_PROMPT,
        )
        .await
    }

    async fn sync_capability(&self, name: &str, available: bool, content: &str) -> Result<()> {
        let path = self.capability_path(name);
        if available {
            write_text(&path, content).await
        } else if path.is_file() {
            tokio::fs::remove_file(path).await?;
            Ok(())
        } else {
            Ok(())
        }
    }

    fn weixin_root(&self) -> PathBuf {
        self.root.join(WEIXIN_CHANNEL)
    }

    fn weixin_runtime_path(&self) -> PathBuf {
        self.weixin_root().join("runtime.yaml")
    }

    fn weixin_secret_path(&self) -> PathBuf {
        self.weixin_root().join("secret.yaml")
    }

    fn telegram_root(&self) -> PathBuf {
        self.root.join(TELEGRAM_CHANNEL)
    }

    fn telegram_runtime_path(&self) -> PathBuf {
        self.telegram_root().join("runtime.yaml")
    }

    fn telegram_secret_path(&self) -> PathBuf {
        self.telegram_root().join("secret.yaml")
    }

    fn capability_path(&self, name: &str) -> PathBuf {
        self.capability_root.join(format!("{name}.md"))
    }
}

pub(crate) fn telegram_bot(token: &str, proxy: Option<&str>) -> Result<Bot> {
    let mut client = reqwest::Client::builder();
    if let Some(proxy) = proxy {
        client = client.proxy(
            reqwest::Proxy::all(proxy)
                .with_context(|| format!("configure Telegram HTTP proxy {proxy}"))?,
        );
    }
    Ok(Bot::with_client(token.to_string(), client.build()?))
}

fn telegram_bind_identity(update: &Update, code: &str) -> Option<(u64, i64)> {
    let UpdateKind::Message(message) = &update.kind else {
        return None;
    };
    let user = message.from.as_ref()?;
    let text = message.text()?.trim();
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    let command = command.split_once('@').map_or(command, |(name, _)| name);
    (message.chat.is_private()
        && !user.is_bot
        && command == "/bind"
        && parts.next() == Some(code)
        && parts.next().is_none())
    .then_some((user.id.0, message.chat.id.0))
}

fn resolve_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("environment variable {name}"))?;
    if value.trim().is_empty() {
        bail!("environment variable {name} is empty");
    }
    Ok(value)
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
    write_text(path, &source).await
}

async fn write_text(path: &Path, source: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
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
        let state = WeixinChannelState {
            selected_session_id: Some("session-test".to_string()),
            sync_buf: Some("cursor".to_string()),
            context_tokens: HashMap::from([("user".to_string(), "token".to_string())]),
            ..WeixinChannelState::default()
        };

        manager.save_weixin_state(&state).await.unwrap();

        let source = tokio::fs::read_to_string(manager.weixin_runtime_path())
            .await
            .unwrap();
        assert!(source.contains("syncBuf: cursor"));
        assert!(source.contains("contextTokens:"));
        assert!(!source.contains("streamMode"));
        assert!(!manager.root.join("state.json").exists());

        manager
            .save_weixin(WeixinSecret {
                bot_token: "secret-token".to_string(),
                base_url: "https://example.test".to_string(),
                ilink_bot_id: "bot".to_string(),
                bound_user_id: "user".to_string(),
            })
            .await
            .unwrap();
        let capability = tokio::fs::read_to_string(manager.capability_path(WEIXIN_CHANNEL))
            .await
            .unwrap();
        assert!(capability.contains("dwo channel weixin send-file"));
        assert!(!capability.contains("secret-token"));
        manager.remove_weixin().await.unwrap();
        assert!(!manager.capability_path(WEIXIN_CHANNEL).exists());
    }

    #[tokio::test]
    async fn telegram_private_chat_secret_and_state_are_persisted_separately() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 4
botTokenEnv: DWO_TEST_MISSING_TELEGRAM_TOKEN
tgProxy: http://127.0.0.1:7890
mediaInput: true
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(TELEGRAM_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let summary = manager.list().await.unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].name, TELEGRAM_CHANNEL);
        assert!(!summary[0].connected);
        assert!(summary[0].bound_user_id.is_none());

        let state = TelegramChannelState {
            selected_session_id: Some("session-test".to_string()),
        };
        manager.save_telegram_state(&state).await.unwrap();
        manager
            .save_telegram(TelegramSecret {
                bot_id: 7,
                bot_username: "dwo_test_bot".to_string(),
                bound_user_id: 12345,
                bound_chat_id: 12345,
            })
            .await
            .unwrap();
        manager.save_telegram_state(&state).await.unwrap();
        let loaded: TelegramChannelState =
            read_yaml(&manager.telegram_runtime_path()).await.unwrap();
        assert_eq!(loaded.selected_session_id, state.selected_session_id);
        let secret_source = tokio::fs::read_to_string(manager.telegram_secret_path())
            .await
            .unwrap();
        assert!(secret_source.contains("boundUserId: 12345"));
        assert!(!secret_source.contains("botToken"));
        let summary = manager.list().await.unwrap();
        assert_eq!(summary[0].bound_user_id.as_deref(), Some("12345"));
        assert!(!summary[0].connected, "the token environment is absent");
        let capability = tokio::fs::read_to_string(manager.capability_path(TELEGRAM_CHANNEL))
            .await
            .unwrap();
        assert!(capability.contains("dwo channel telegram send-file"));
        assert!(!capability.contains("DWO_TEST_MISSING_TELEGRAM_TOKEN"));
        manager.remove_telegram().await.unwrap();
        assert!(!manager.capability_path(TELEGRAM_CHANNEL).exists());
    }

    #[tokio::test]
    async fn telegram_rejects_an_empty_proxy_value() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 4
botTokenEnv: TELEGRAM_BOT_TOKEN
tgProxy: ""
mediaInput: true
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(TELEGRAM_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let error = ChannelManager::new(root.path(), &channels)
            .await
            .err()
            .expect("empty Telegram proxy should fail");

        assert!(format!("{error:#}").contains("tgProxy"));
    }

    #[test]
    fn telegram_binding_accepts_only_the_exact_private_command() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "message_id": 9,
            "date": 1569518342,
            "chat": {
                "id": 12345,
                "type": "private",
                "first_name": "User",
                "username": "test_user"
            },
            "from": {
                "id": 12345,
                "is_bot": false,
                "first_name": "User",
                "username": "test_user",
                "language_code": "en"
            },
            "text": "/bind@dwo_bot A1B2C3D4",
            "link_preview_options": {"is_disabled": true}
        }))
        .unwrap();
        let update = Update {
            id: teloxide::types::UpdateId(7),
            kind: UpdateKind::Message(message),
        };
        let UpdateKind::Message(message) = &update.kind else {
            panic!("expected Telegram message update, got {:#?}", update.kind);
        };
        assert_eq!(message.text(), Some("/bind@dwo_bot A1B2C3D4"));

        assert_eq!(
            telegram_bind_identity(&update, "A1B2C3D4"),
            Some((12345, 12345))
        );
        assert_eq!(telegram_bind_identity(&update, "WRONG"), None);
    }
}
