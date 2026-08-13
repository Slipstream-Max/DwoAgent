use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dwo_context::ChannelCapabilitySnapshot;
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::UpdateKind;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;
use weixin_agent::{LoginStatus, QrLoginSession, StandaloneQrLogin, WeixinConfig};

use super::ChannelKind;

const WEIXIN_CHANNEL: &str = ChannelKind::Weixin.as_str();
const TELEGRAM_CHANNEL: &str = ChannelKind::Telegram.as_str();
const FEISHU_CHANNEL: &str = ChannelKind::Feishu.as_str();
const QQ_CHANNEL: &str = ChannelKind::Qq.as_str();
const WEBSOCKET_CHANNEL: &str = ChannelKind::Websocket.as_str();

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebsocketChannelConfig {
    pub enabled: bool,
    pub port: u16,
}

impl WebsocketChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.port == 0 {
            bail!("channels.websocket.port must be greater than 0");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WeixinChannelConfig {
    pub enabled: bool,
    pub replay_turns: usize,
    #[serde(default)]
    pub output_mode: ChannelOutputMode,
    pub markdown_filter: bool,
    #[serde(default = "default_true")]
    pub media_input: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ChannelOutputMode {
    #[default]
    Final,
    Full,
}

impl WeixinChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.weixin.replayTurns must be at most 10");
        }
        if self.output_mode != ChannelOutputMode::Final {
            bail!("channels.weixin.outputMode only supports final");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelState<T = EmptyChannelState> {
    pub selected_session_id: Option<String>,
    #[serde(flatten)]
    pub adapter: T,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmptyChannelState {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WeixinTransportState {
    pub sync_buf: Option<String>,
    pub context_tokens: HashMap<String, String>,
}

pub type WeixinChannelState = ChannelState<WeixinTransportState>;
pub type TelegramChannelState = ChannelState;
pub type FeishuChannelState = ChannelState;
pub type QqChannelState = ChannelState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TelegramChannelConfig {
    pub enabled: bool,
    pub replay_turns: usize,
    #[serde(default)]
    pub output_mode: ChannelOutputMode,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FeishuPlatform {
    Feishu,
    Lark,
}

impl FeishuPlatform {
    pub(crate) fn base_url(self) -> &'static str {
        match self {
            Self::Feishu => "https://open.feishu.cn",
            Self::Lark => "https://open.larksuite.com",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeishuChannelConfig {
    pub enabled: bool,
    pub replay_turns: usize,
    #[serde(default)]
    pub output_mode: ChannelOutputMode,
    pub app_id_env: String,
    pub app_secret_env: String,
    pub(crate) platform: FeishuPlatform,
    #[serde(default = "default_true")]
    pub media_input: bool,
}

impl FeishuChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.feishu.replayTurns must be at most 10");
        }
        if self.app_id_env.trim().is_empty() {
            bail!("channels.feishu.appIdEnv must not be empty");
        }
        if self.app_secret_env.trim().is_empty() {
            bail!("channels.feishu.appSecretEnv must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QqChannelConfig {
    pub enabled: bool,
    pub replay_turns: usize,
    #[serde(default)]
    pub output_mode: ChannelOutputMode,
    #[serde(default = "default_true")]
    pub media_input: bool,
}

impl QqChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.qq.replayTurns must be at most 10");
        }
        Ok(())
    }
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
pub(crate) struct FeishuSecret {
    pub(crate) bound_open_id: String,
    pub(crate) bound_chat_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct QqSecret {
    pub(crate) app_id: String,
    pub(crate) app_secret: String,
    pub(crate) bound_user_openid: String,
}

impl QqSecret {
    fn validate(&self) -> Result<()> {
        if self.app_id.trim().is_empty() {
            bail!("channels.qq secret field appId must not be empty");
        }
        if self.app_secret.trim().is_empty() {
            bail!("channels.qq secret field appSecret must not be empty");
        }
        if self.bound_user_openid.trim().is_empty() {
            bail!("channels.qq secret field boundUserOpenid must not be empty");
        }
        Ok(())
    }
}

impl FeishuSecret {
    fn validate(&self) -> Result<()> {
        if self.bound_open_id.trim().is_empty() {
            bail!("channels.feishu secret field boundOpenId must not be empty");
        }
        if self.bound_chat_id.trim().is_empty() {
            bail!("channels.feishu secret field boundChatId must not be empty");
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WebsocketSecret {
    pub(crate) token: String,
}

impl WebsocketSecret {
    fn validate(&self) -> Result<()> {
        if self.token.trim().is_empty() {
            bail!("channels.websocket secret field token must not be empty");
        }
        Ok(())
    }
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

pub(crate) struct FeishuRuntime {
    pub(crate) config: FeishuChannelConfig,
    pub(crate) state: FeishuChannelState,
    pub(crate) secret: FeishuSecret,
    pub(crate) app_id: String,
    pub(crate) app_secret: String,
}

pub(crate) struct QqRuntime {
    pub(crate) config: QqChannelConfig,
    pub(crate) state: QqChannelState,
    pub(crate) secret: QqSecret,
    pub(crate) app_id: String,
    pub(crate) app_secret: String,
}

pub(crate) struct WebsocketRuntime {
    pub(crate) config: WebsocketChannelConfig,
    pub(crate) token: String,
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

#[derive(Debug, Clone, Serialize)]
pub struct FeishuBindStart {
    pub binding_id: String,
    pub code: String,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FeishuBindProgress {
    Waiting,
    Confirmed { channel: ChannelSummary },
    Expired,
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct QqBindStart {
    pub binding_id: String,
    pub qrcode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QqBindProgress {
    Waiting,
    Confirmed { channel: ChannelSummary },
    Expired,
    Failed { message: String },
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

struct PendingFeishuBind {
    config: open_lark::Config,
    code: String,
    receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
    expires_at: Instant,
}

struct PendingQqBind {
    task_id: String,
    key: String,
    expires_at: Instant,
}

#[derive(Clone)]
struct ChannelStore {
    root: PathBuf,
}

impl ChannelStore {
    fn new(root: &Path, channel: ChannelKind) -> Self {
        Self {
            root: root.join(channel.as_str()),
        }
    }

    fn runtime_path(&self) -> PathBuf {
        self.root.join("runtime.yaml")
    }

    fn secret_path(&self) -> PathBuf {
        self.root.join("secret.yaml")
    }

    async fn load_runtime<T: serde::de::DeserializeOwned + Default>(&self) -> Result<T> {
        read_yaml_or_default(&self.runtime_path()).await
    }

    async fn load_secret<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        read_yaml(&self.secret_path()).await
    }

    async fn save_runtime(&self, state: &impl Serialize) -> Result<()> {
        self.save(&self.runtime_path(), state).await
    }

    async fn save_secret(&self, secret: &impl Serialize) -> Result<()> {
        self.save(&self.secret_path(), secret).await
    }

    async fn save(&self, path: &Path, value: &impl Serialize) -> Result<()> {
        tokio::fs::create_dir_all(&self.root).await?;
        write_yaml(path, value).await?;
        set_private_permissions(path).await
    }

    async fn remove(&self) -> Result<bool> {
        let paths = [self.secret_path(), self.runtime_path()];
        let existed = paths.iter().any(|path| path.is_file());
        for path in paths {
            if path.is_file() {
                tokio::fs::remove_file(path).await?;
            }
        }
        Ok(existed)
    }
}

pub struct ChannelManager {
    profile_root: PathBuf,
    root: PathBuf,
    weixin: Option<WeixinChannelConfig>,
    telegram: Option<TelegramChannelConfig>,
    feishu: Option<FeishuChannelConfig>,
    qq: Option<QqChannelConfig>,
    websocket: Option<WebsocketChannelConfig>,
    pending_weixin: Mutex<HashMap<String, Arc<Mutex<PendingLogin>>>>,
    pending_telegram: Mutex<HashMap<String, PendingTelegramBind>>,
    pending_feishu: Mutex<HashMap<String, PendingFeishuBind>>,
    pending_qq: Mutex<HashMap<String, PendingQqBind>>,
    capabilities: Mutex<BTreeMap<String, String>>,
}

impl ChannelManager {
    pub async fn new(
        profile_root: &Path,
        channels: &BTreeMap<String, serde_yaml::Value>,
    ) -> Result<Self> {
        if let Some(unsupported) = channels.keys().find(|name| {
            !matches!(
                name.as_str(),
                WEIXIN_CHANNEL | TELEGRAM_CHANNEL | FEISHU_CHANNEL | QQ_CHANNEL | WEBSOCKET_CHANNEL
            )
        }) {
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
        let feishu = channels
            .get(FEISHU_CHANNEL)
            .cloned()
            .map(serde_yaml::from_value::<FeishuChannelConfig>)
            .transpose()
            .context("parse channels.feishu")?;
        if let Some(config) = &feishu {
            config.validate()?;
        }
        let qq = channels
            .get(QQ_CHANNEL)
            .cloned()
            .map(serde_yaml::from_value::<QqChannelConfig>)
            .transpose()
            .context("parse channels.qq")?;
        if let Some(config) = &qq {
            config.validate()?;
        }
        let websocket = channels
            .get(WEBSOCKET_CHANNEL)
            .cloned()
            .map(serde_yaml::from_value::<WebsocketChannelConfig>)
            .transpose()
            .context("parse channels.websocket")?;
        if let Some(config) = &websocket {
            config.validate()?;
        }
        let root = profile_root.join("channels");
        tokio::fs::create_dir_all(&root).await?;
        let manager = Self {
            profile_root: profile_root.to_path_buf(),
            root,
            weixin,
            telegram,
            feishu,
            qq,
            websocket,
            pending_weixin: Mutex::new(HashMap::new()),
            pending_telegram: Mutex::new(HashMap::new()),
            pending_feishu: Mutex::new(HashMap::new()),
            pending_qq: Mutex::new(HashMap::new()),
            capabilities: Mutex::new(BTreeMap::new()),
        };
        manager.sync_capabilities().await?;
        Ok(manager)
    }

    pub async fn list(&self) -> Result<Vec<ChannelSummary>> {
        let configured = [
            (ChannelKind::Weixin, self.weixin.is_some()),
            (ChannelKind::Telegram, self.telegram.is_some()),
            (ChannelKind::Feishu, self.feishu.is_some()),
            (ChannelKind::Qq, self.qq.is_some()),
            (ChannelKind::Websocket, self.websocket.is_some()),
        ];
        let mut summaries = Vec::with_capacity(configured.len());
        for (channel, is_configured) in configured {
            if is_configured {
                summaries.push(self.summary(channel).await?);
            }
        }
        Ok(summaries)
    }

    pub async fn summary(&self, channel: ChannelKind) -> Result<ChannelSummary> {
        let store = self.store(channel);
        match channel {
            ChannelKind::Weixin => {
                let config = self
                    .weixin
                    .as_ref()
                    .context("channels.weixin is not configured")?;
                let state: WeixinChannelState = store.load_runtime().await?;
                let (connected, bound_user_id) = if store.secret_path().is_file() {
                    let secret = store.load_secret::<WeixinSecret>().await?;
                    secret.validate()?;
                    (true, Some(secret.bound_user_id))
                } else {
                    (false, None)
                };
                Ok(ChannelSummary {
                    name: WEIXIN_CHANNEL.to_string(),
                    enabled: config.enabled,
                    connected,
                    selected_session_id: state.selected_session_id,
                    bound_user_id,
                })
            }
            ChannelKind::Telegram => {
                let config = self
                    .telegram
                    .as_ref()
                    .context("channels.telegram is not configured")?;
                let state: TelegramChannelState = store.load_runtime().await?;
                let (connected, bound_user_id) = if store.secret_path().is_file() {
                    let secret = store.load_secret::<TelegramSecret>().await?;
                    secret.validate()?;
                    (
                        resolve_env(&config.bot_token_env).is_ok(),
                        Some(secret.bound_user_id.to_string()),
                    )
                } else {
                    (false, None)
                };
                Ok(ChannelSummary {
                    name: TELEGRAM_CHANNEL.to_string(),
                    enabled: config.enabled,
                    connected,
                    selected_session_id: state.selected_session_id,
                    bound_user_id,
                })
            }
            ChannelKind::Feishu => {
                let config = self
                    .feishu
                    .as_ref()
                    .context("channels.feishu is not configured")?;
                let state: FeishuChannelState = store.load_runtime().await?;
                let (connected, bound_user_id) = if store.secret_path().is_file() {
                    let secret = store.load_secret::<FeishuSecret>().await?;
                    secret.validate()?;
                    (
                        resolve_env(&config.app_id_env).is_ok()
                            && resolve_env(&config.app_secret_env).is_ok(),
                        Some(secret.bound_open_id),
                    )
                } else {
                    (false, None)
                };
                Ok(ChannelSummary {
                    name: FEISHU_CHANNEL.to_string(),
                    enabled: config.enabled,
                    connected,
                    selected_session_id: state.selected_session_id,
                    bound_user_id,
                })
            }
            ChannelKind::Qq => {
                let config = self.qq.as_ref().context("channels.qq is not configured")?;
                let state: QqChannelState = store.load_runtime().await?;
                let (connected, bound_user_id) = if store.secret_path().is_file() {
                    let secret = store.load_secret::<QqSecret>().await?;
                    secret.validate()?;
                    (true, Some(secret.bound_user_openid))
                } else {
                    (false, None)
                };
                Ok(ChannelSummary {
                    name: QQ_CHANNEL.to_string(),
                    enabled: config.enabled,
                    connected,
                    selected_session_id: state.selected_session_id,
                    bound_user_id,
                })
            }
            ChannelKind::Websocket => {
                let config = self
                    .websocket
                    .as_ref()
                    .context("channels.websocket is not configured")?;
                let connected = if config.enabled {
                    self.ensure_websocket_secret().await?;
                    true
                } else {
                    store.secret_path().is_file()
                };
                Ok(ChannelSummary {
                    name: WEBSOCKET_CHANNEL.to_string(),
                    enabled: config.enabled,
                    connected,
                    selected_session_id: None,
                    bound_user_id: None,
                })
            }
        }
    }

    pub(crate) async fn bound_target(&self, channel: ChannelKind) -> Result<String> {
        match channel {
            ChannelKind::Weixin => Ok(self.load_weixin().await?.secret.bound_user_id),
            ChannelKind::Telegram => {
                Ok(self.load_telegram().await?.secret.bound_chat_id.to_string())
            }
            ChannelKind::Feishu => Ok(self.load_feishu().await?.secret.bound_chat_id),
            ChannelKind::Qq => Ok(self.load_qq().await?.secret.bound_user_openid),
            ChannelKind::Websocket => bail!("WebSocket channel has no bound target"),
        }
    }

    pub async fn remove(&self, channel: ChannelKind) -> Result<bool> {
        let removed = self.store(channel).remove().await?;
        self.sync_channel_capability(channel).await?;
        Ok(removed)
    }

    pub(crate) async fn load_weixin(&self) -> Result<WeixinRuntime> {
        let config = self
            .weixin
            .clone()
            .context("channels.weixin is not configured")?;
        let store = self.store(ChannelKind::Weixin);
        let secret: WeixinSecret = store.load_secret().await?;
        secret.validate()?;
        Ok(WeixinRuntime {
            state: store.load_runtime().await?,
            secret,
            config,
        })
    }

    pub(crate) async fn save_state(
        &self,
        channel: ChannelKind,
        state: &impl Serialize,
    ) -> Result<()> {
        self.store(channel).save_runtime(state).await
    }

    pub(crate) async fn load_telegram(&self) -> Result<TelegramRuntime> {
        let config = self
            .telegram
            .clone()
            .context("channels.telegram is not configured")?;
        let bot_token = resolve_env(&config.bot_token_env)
            .with_context(|| format!("resolve channels.telegram.{}", config.bot_token_env))?;
        let store = self.store(ChannelKind::Telegram);
        let secret: TelegramSecret = store.load_secret().await?;
        secret.validate()?;
        Ok(TelegramRuntime {
            state: store.load_runtime().await?,
            config,
            secret,
            bot_token,
        })
    }

    pub(crate) async fn load_feishu(&self) -> Result<FeishuRuntime> {
        let config = self
            .feishu
            .clone()
            .context("channels.feishu is not configured")?;
        let app_id = resolve_env(&config.app_id_env)
            .with_context(|| format!("resolve channels.feishu.{}", config.app_id_env))?;
        let app_secret = resolve_env(&config.app_secret_env)
            .with_context(|| format!("resolve channels.feishu.{}", config.app_secret_env))?;
        let store = self.store(ChannelKind::Feishu);
        let secret: FeishuSecret = store.load_secret().await?;
        secret.validate()?;
        Ok(FeishuRuntime {
            state: store.load_runtime().await?,
            config,
            secret,
            app_id,
            app_secret,
        })
    }

    pub(crate) async fn load_qq(&self) -> Result<QqRuntime> {
        let config = self.qq.clone().context("channels.qq is not configured")?;
        let store = self.store(ChannelKind::Qq);
        let secret: QqSecret = store.load_secret().await?;
        secret.validate()?;
        Ok(QqRuntime {
            state: store.load_runtime().await?,
            app_id: secret.app_id.clone(),
            app_secret: secret.app_secret.clone(),
            secret,
            config,
        })
    }

    pub(crate) async fn load_websocket(&self) -> Result<WebsocketRuntime> {
        let config = self
            .websocket
            .clone()
            .context("channels.websocket is not configured")?;
        let secret = self.ensure_websocket_secret().await?;
        Ok(WebsocketRuntime {
            config,
            token: secret.token,
        })
    }

    pub async fn reset_websocket_token(&self) -> Result<String> {
        self.websocket
            .as_ref()
            .context("channels.websocket is not configured")?;
        let secret = new_websocket_secret()?;
        self.store(ChannelKind::Websocket)
            .save_secret(&secret)
            .await?;
        Ok(secret.token)
    }

    async fn ensure_websocket_secret(&self) -> Result<WebsocketSecret> {
        let store = self.store(ChannelKind::Websocket);
        if store.secret_path().is_file() {
            let secret: WebsocketSecret = store.load_secret().await?;
            secret.validate()?;
            return Ok(secret);
        }
        let secret = new_websocket_secret()?;
        store.save_secret(&secret).await?;
        Ok(secret)
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

    pub async fn begin_feishu_bind(&self) -> Result<FeishuBindStart> {
        let settings = self
            .feishu
            .as_ref()
            .context("channels.feishu is not configured in profile.yaml")?;
        let app_id = resolve_env(&settings.app_id_env)
            .with_context(|| format!("resolve channels.feishu.{}", settings.app_id_env))?;
        let app_secret = resolve_env(&settings.app_secret_env)
            .with_context(|| format!("resolve channels.feishu.{}", settings.app_secret_env))?;
        let config = super::feishu::openlark_config(settings, app_id, app_secret);
        super::feishu::validate_credentials(&config).await?;

        let binding_id = format!("binding-{}", Uuid::new_v4());
        let code = Uuid::new_v4().simple().to_string()[..8].to_ascii_uppercase();
        let (sender, receiver) = mpsc::unbounded_channel();
        let handler = open_lark::ws_client::EventDispatcherHandler::builder()
            .payload_sender(sender)
            .build();
        let connection_config = config.clone();
        let task = tokio::spawn(async move {
            open_lark::ws_client::LarkWsClient::open(Arc::new(connection_config), handler)
                .await
                .map_err(|error| error.to_string())
        });

        let mut pending = self.pending_feishu.lock().await;
        for (_, old) in pending.drain() {
            old.task.abort();
        }
        pending.insert(
            binding_id.clone(),
            PendingFeishuBind {
                config,
                code: code.clone(),
                receiver,
                task,
                expires_at: Instant::now() + Duration::from_secs(10 * 60),
            },
        );
        Ok(FeishuBindStart {
            binding_id,
            code,
            platform: match settings.platform {
                FeishuPlatform::Feishu => "feishu",
                FeishuPlatform::Lark => "lark",
            }
            .to_string(),
        })
    }

    pub async fn poll_feishu_bind(&self, binding_id: &str) -> Result<FeishuBindProgress> {
        let mut pending = self.pending_feishu.lock().await;
        let binding = pending
            .get_mut(binding_id)
            .ok_or_else(|| anyhow::anyhow!("unknown or completed binding: {binding_id}"))?;
        if Instant::now() >= binding.expires_at {
            let binding = pending
                .remove(binding_id)
                .expect("checked Feishu binding exists");
            binding.task.abort();
            return Ok(FeishuBindProgress::Expired);
        }

        let mut identity = None;
        while let Ok(payload) = binding.receiver.try_recv() {
            if let Some(found) = super::feishu::bind_identity(&payload, &binding.code) {
                identity = Some(found);
                break;
            }
        }
        if let Some((bound_open_id, bound_chat_id)) = identity {
            let binding = pending
                .remove(binding_id)
                .expect("checked Feishu binding exists");
            binding.task.abort();
            drop(pending);
            self.save_feishu(FeishuSecret {
                bound_open_id: bound_open_id.clone(),
                bound_chat_id,
            })
            .await?;
            super::feishu::send_text_to(
                &binding.config,
                &bound_open_id,
                "Feishu channel bound to this private chat",
            )
            .await?;
            let channel = self
                .list()
                .await?
                .into_iter()
                .find(|channel| channel.name == FEISHU_CHANNEL)
                .expect("configured Feishu channel is listed");
            return Ok(FeishuBindProgress::Confirmed { channel });
        }

        if binding.task.is_finished() {
            let binding = pending
                .remove(binding_id)
                .expect("checked Feishu binding exists");
            drop(pending);
            let message = match binding.task.await {
                Ok(Ok(())) => "Feishu long connection closed".to_string(),
                Ok(Err(error)) => error,
                Err(error) => error.to_string(),
            };
            return Ok(FeishuBindProgress::Failed { message });
        }
        Ok(FeishuBindProgress::Waiting)
    }

    pub async fn begin_qq_bind(&self) -> Result<QqBindStart> {
        self.qq
            .as_ref()
            .context("channels.qq is not configured in profile.yaml")?;
        let task = super::qq::create_bind_task().await?;
        let binding_id = format!("binding-{}", Uuid::new_v4());
        let qrcode = super::qq::bind_qr_url(&task.task_id);
        let mut pending = self.pending_qq.lock().await;
        pending.clear();
        pending.insert(
            binding_id.clone(),
            PendingQqBind {
                task_id: task.task_id,
                key: task.key,
                expires_at: Instant::now() + Duration::from_secs(10 * 60),
            },
        );
        Ok(QqBindStart { binding_id, qrcode })
    }

    pub async fn poll_qq_bind(&self, binding_id: &str) -> Result<QqBindProgress> {
        let (task_id, key) = {
            let mut pending = self.pending_qq.lock().await;
            let binding = pending
                .get(binding_id)
                .ok_or_else(|| anyhow::anyhow!("unknown or completed binding: {binding_id}"))?;
            if Instant::now() >= binding.expires_at {
                pending.remove(binding_id);
                return Ok(QqBindProgress::Expired);
            }
            (binding.task_id.clone(), binding.key.clone())
        };

        match super::qq::poll_bind_task(&task_id, &key).await? {
            super::qq::QqQrPoll::Waiting => Ok(QqBindProgress::Waiting),
            super::qq::QqQrPoll::Expired => {
                self.pending_qq.lock().await.remove(binding_id);
                Ok(QqBindProgress::Expired)
            }
            super::qq::QqQrPoll::Completed {
                app_id,
                app_secret,
                user_openid,
            } => {
                self.pending_qq.lock().await.remove(binding_id);
                let Some(bound_user_openid) = user_openid.filter(|value| !value.trim().is_empty())
                else {
                    return Ok(QqBindProgress::Failed {
                        message: "QQ QR binding did not return userOpenid; single-user binding cannot be established"
                            .to_string(),
                    });
                };
                if let Err(error) = super::qq::validate_credentials(&app_id, &app_secret).await {
                    return Ok(QqBindProgress::Failed {
                        message: format!("validate QQ credentials: {error:#}"),
                    });
                }
                self.save_qq(QqSecret {
                    app_id,
                    app_secret,
                    bound_user_openid,
                })
                .await?;
                let channel = self.summary(ChannelKind::Qq).await?;
                Ok(QqBindProgress::Confirmed { channel })
            }
        }
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
        self.pending_weixin.lock().await.insert(
            binding_id.clone(),
            Arc::new(Mutex::new(PendingLogin { session, config })),
        );
        Ok(WeixinLoginStart { binding_id, qrcode })
    }

    pub async fn poll_weixin_login(
        &self,
        binding_id: &str,
        verify_code: Option<&str>,
    ) -> Result<WeixinLoginProgress> {
        let login_slot = self
            .pending_weixin
            .lock()
            .await
            .get(binding_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown or completed binding: {binding_id}"))?;
        let login = login_slot.lock().await;
        let api = StandaloneQrLogin::new(&login.config);
        let status = api.poll_status(&login.session, verify_code).await?;
        let (progress, completed) = match status {
            LoginStatus::Wait | LoginStatus::ScannedButRedirect { .. } => {
                (WeixinLoginProgress::Waiting, false)
            }
            LoginStatus::Scanned => (WeixinLoginProgress::Scanned, false),
            LoginStatus::Expired => (WeixinLoginProgress::Expired, true),
            LoginStatus::NeedVerifyCode => (WeixinLoginProgress::NeedVerifyCode, false),
            LoginStatus::Confirmed {
                bot_token,
                ilink_bot_id,
                base_url,
                ilink_user_id,
            } => {
                self.save_weixin(WeixinSecret {
                    bot_token,
                    base_url,
                    ilink_bot_id,
                    bound_user_id: ilink_user_id,
                })
                .await?;
                let channel = self.summary(ChannelKind::Weixin).await?;
                (WeixinLoginProgress::Confirmed { channel }, true)
            }
            LoginStatus::VerifyCodeBlocked | LoginStatus::BindedRedirect => (
                WeixinLoginProgress::Failed {
                    message: "Weixin rejected this QR login".to_string(),
                },
                true,
            ),
            _ => (WeixinLoginProgress::Waiting, false),
        };
        drop(login);
        if completed {
            let mut pending = self.pending_weixin.lock().await;
            if pending
                .get(binding_id)
                .is_some_and(|current| Arc::ptr_eq(current, &login_slot))
            {
                pending.remove(binding_id);
            }
        }
        Ok(progress)
    }

    async fn save_weixin(&self, secret: WeixinSecret) -> Result<()> {
        secret.validate()?;
        self.save_binding(ChannelKind::Weixin, &WeixinChannelState::default(), &secret)
            .await
    }

    async fn save_telegram(&self, secret: TelegramSecret) -> Result<()> {
        secret.validate()?;
        self.save_binding(
            ChannelKind::Telegram,
            &TelegramChannelState::default(),
            &secret,
        )
        .await
    }

    async fn save_feishu(&self, secret: FeishuSecret) -> Result<()> {
        secret.validate()?;
        self.save_binding(ChannelKind::Feishu, &FeishuChannelState::default(), &secret)
            .await
    }

    async fn save_qq(&self, secret: QqSecret) -> Result<()> {
        secret.validate()?;
        self.save_binding(ChannelKind::Qq, &QqChannelState::default(), &secret)
            .await
    }

    async fn save_binding(
        &self,
        channel: ChannelKind,
        state: &impl Serialize,
        secret: &impl Serialize,
    ) -> Result<()> {
        let store = self.store(channel);
        store.save_runtime(state).await?;
        store.save_secret(secret).await?;
        self.sync_channel_capability(channel).await
    }

    async fn existing_weixin_tokens(&self) -> Result<Vec<String>> {
        let store = self.store(ChannelKind::Weixin);
        if !store.secret_path().is_file() {
            return Ok(Vec::new());
        }
        let secret: WeixinSecret = store.load_secret().await?;
        secret.validate()?;
        Ok(vec![secret.bot_token])
    }

    async fn sync_capabilities(&self) -> Result<()> {
        for channel in ChannelKind::ALL {
            self.sync_channel_capability(channel).await?;
        }
        Ok(())
    }

    async fn sync_channel_capability(&self, channel: ChannelKind) -> Result<()> {
        let store = self.store(channel);
        let (available, content) = match channel {
            ChannelKind::Weixin => (
                self.weixin.as_ref().is_some_and(|config| config.enabled)
                    && store
                        .load_secret::<WeixinSecret>()
                        .await
                        .is_ok_and(|secret| secret.validate().is_ok()),
                super::weixin::CAPABILITY_PROMPT,
            ),
            ChannelKind::Telegram => (
                self.telegram.as_ref().is_some_and(|config| config.enabled)
                    && store
                        .load_secret::<TelegramSecret>()
                        .await
                        .is_ok_and(|secret| secret.validate().is_ok()),
                super::telegram::CAPABILITY_PROMPT,
            ),
            ChannelKind::Feishu => {
                let available =
                    if let Some(config) = self.feishu.as_ref().filter(|config| config.enabled) {
                        store
                            .load_secret::<FeishuSecret>()
                            .await
                            .is_ok_and(|secret| secret.validate().is_ok())
                            && resolve_env(&config.app_id_env).is_ok()
                            && resolve_env(&config.app_secret_env).is_ok()
                    } else {
                        false
                    };
                (available, super::feishu::CAPABILITY_PROMPT)
            }
            ChannelKind::Qq => (
                self.qq.as_ref().is_some_and(|config| config.enabled)
                    && store
                        .load_secret::<QqSecret>()
                        .await
                        .is_ok_and(|secret| secret.validate().is_ok()),
                super::qq::CAPABILITY_PROMPT,
            ),
            ChannelKind::Websocket => (false, ""),
        };
        self.sync_capability(channel.as_str(), available, content)
            .await
    }

    async fn sync_capability(&self, name: &str, available: bool, content: &str) -> Result<()> {
        let mut capabilities = self.capabilities.lock().await;
        if available {
            capabilities.insert(name.to_string(), content.to_string());
        } else {
            capabilities.remove(name);
        }
        let current = capabilities
            .iter()
            .map(|(name, content)| ChannelCapabilitySnapshot {
                name: name.clone(),
                content: content.clone(),
            })
            .collect();
        ChannelCapabilitySnapshot::set_runtime(&self.profile_root, current);
        Ok(())
    }

    fn store(&self, channel: ChannelKind) -> ChannelStore {
        ChannelStore::new(&self.root, channel)
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

fn new_websocket_secret() -> Result<WebsocketSecret> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).context("generate WebSocket token")?;
    Ok(WebsocketSecret {
        token: URL_SAFE_NO_PAD.encode(bytes),
    })
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
    dwo_agent_service::atomic_file::write(path, source.as_bytes().to_vec()).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_mode_is_the_only_supported_channel_output_field() {
        let full = serde_yaml::from_str::<QqChannelConfig>(
            "enabled: true\nreplayTurns: 1\noutputMode: full\nmediaInput: true\n",
        )
        .unwrap();
        assert_eq!(full.output_mode, ChannelOutputMode::Full);

        let old = serde_yaml::from_str::<QqChannelConfig>(
            "enabled: true\nreplayTurns: 1\nreplayMode: full\nmediaInput: true\n",
        );
        assert!(old.is_err(), "replayMode must not be accepted");
    }

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
            adapter: WeixinTransportState {
                sync_buf: Some("cursor".to_string()),
                context_tokens: HashMap::from([("user".to_string(), "token".to_string())]),
            },
        };

        manager
            .save_state(ChannelKind::Weixin, &state)
            .await
            .unwrap();

        let source = tokio::fs::read_to_string(manager.store(ChannelKind::Weixin).runtime_path())
            .await
            .unwrap();
        assert!(source.contains("syncBuf: cursor"));
        assert!(source.contains("contextTokens:"));
        assert!(!source.contains("adapter:"));
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
        let capability = dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
            .into_iter()
            .find(|capability| capability.name == WEIXIN_CHANNEL)
            .unwrap()
            .content;
        assert!(capability.contains("dwo channel weixin send-file"));
        assert!(!capability.contains("secret-token"));
        manager.remove(ChannelKind::Weixin).await.unwrap();
        assert!(
            dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
                .into_iter()
                .all(|capability| capability.name != WEIXIN_CHANNEL)
        );
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
            ..Default::default()
        };
        manager
            .save_state(ChannelKind::Telegram, &state)
            .await
            .unwrap();
        manager
            .save_telegram(TelegramSecret {
                bot_id: 7,
                bot_username: "dwo_test_bot".to_string(),
                bound_user_id: 12345,
                bound_chat_id: 12345,
            })
            .await
            .unwrap();
        manager
            .save_state(ChannelKind::Telegram, &state)
            .await
            .unwrap();
        let store = manager.store(ChannelKind::Telegram);
        let loaded: TelegramChannelState = read_yaml(&store.runtime_path()).await.unwrap();
        assert_eq!(loaded.selected_session_id, state.selected_session_id);
        let secret_source = tokio::fs::read_to_string(store.secret_path())
            .await
            .unwrap();
        assert!(secret_source.contains("boundUserId: 12345"));
        assert!(!secret_source.contains("botToken"));
        let summary = manager.list().await.unwrap();
        assert_eq!(summary[0].bound_user_id.as_deref(), Some("12345"));
        assert!(!summary[0].connected, "the token environment is absent");
        let capability = dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
            .into_iter()
            .find(|capability| capability.name == TELEGRAM_CHANNEL)
            .unwrap()
            .content;
        assert!(capability.contains("dwo channel telegram send-file"));
        assert!(!capability.contains("DWO_TEST_MISSING_TELEGRAM_TOKEN"));
        manager.remove(ChannelKind::Telegram).await.unwrap();
        assert!(
            dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
                .into_iter()
                .all(|capability| capability.name != TELEGRAM_CHANNEL)
        );
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

    #[tokio::test]
    async fn websocket_token_is_generated_persisted_and_reset() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
port: 8765
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(WEBSOCKET_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let first = manager.load_websocket().await.unwrap();
        let second = manager.load_websocket().await.unwrap();
        assert_eq!(first.config.port, 8765);
        assert_eq!(first.token, second.token);
        assert_eq!(first.token.len(), 43);
        assert!(!first.token.contains('='));

        let reset = manager.reset_websocket_token().await.unwrap();
        assert_ne!(reset, first.token);
        assert_eq!(manager.load_websocket().await.unwrap().token, reset);
        let secret: WebsocketSecret =
            read_yaml(&manager.store(ChannelKind::Websocket).secret_path())
                .await
                .unwrap();
        assert_eq!(secret.token, reset);
    }

    #[tokio::test]
    async fn feishu_private_identity_and_session_state_are_persisted_separately() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
appIdEnv: DWO_TEST_MISSING_FEISHU_APP_ID
appSecretEnv: DWO_TEST_MISSING_FEISHU_APP_SECRET
platform: lark
mediaInput: true
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(FEISHU_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();
        assert_eq!(
            manager.feishu.as_ref().unwrap().platform.base_url(),
            "https://open.larksuite.com"
        );

        manager
            .save_feishu(FeishuSecret {
                bound_open_id: "ou_user".to_string(),
                bound_chat_id: "oc_chat".to_string(),
            })
            .await
            .unwrap();
        let state = FeishuChannelState {
            selected_session_id: Some("session-test".to_string()),
            ..Default::default()
        };
        manager
            .save_state(ChannelKind::Feishu, &state)
            .await
            .unwrap();

        let store = manager.store(ChannelKind::Feishu);
        let loaded: FeishuChannelState = read_yaml(&store.runtime_path()).await.unwrap();
        assert_eq!(loaded.selected_session_id, state.selected_session_id);
        let secret_source = tokio::fs::read_to_string(store.secret_path())
            .await
            .unwrap();
        assert!(secret_source.contains("boundOpenId: ou_user"));
        assert!(secret_source.contains("boundChatId: oc_chat"));
        assert!(!secret_source.contains("appSecret"));
        let summary = manager.list().await.unwrap();
        assert_eq!(summary[0].bound_user_id.as_deref(), Some("ou_user"));
        assert!(
            !summary[0].connected,
            "the credential environment is absent"
        );
        assert!(
            dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
                .into_iter()
                .all(|capability| capability.name != FEISHU_CHANNEL)
        );

        manager.remove(ChannelKind::Feishu).await.unwrap();
        assert!(!store.secret_path().exists());
        assert!(!store.runtime_path().exists());
    }

    #[tokio::test]
    async fn qq_qr_secret_and_session_state_are_persisted_separately() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
mediaInput: true
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(QQ_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        manager
            .save_qq(QqSecret {
                app_id: "102000000".to_string(),
                app_secret: "qq-secret".to_string(),
                bound_user_openid: "qq-user-openid".to_string(),
            })
            .await
            .unwrap();
        let state = QqChannelState {
            selected_session_id: Some("session-test".to_string()),
            ..Default::default()
        };
        manager.save_state(ChannelKind::Qq, &state).await.unwrap();

        let store = manager.store(ChannelKind::Qq);
        let loaded: QqChannelState = read_yaml(&store.runtime_path()).await.unwrap();
        assert_eq!(loaded.selected_session_id, state.selected_session_id);
        let secret_source = tokio::fs::read_to_string(store.secret_path())
            .await
            .unwrap();
        assert!(secret_source.contains("appId: '102000000'"));
        assert!(secret_source.contains("appSecret: qq-secret"));
        assert!(secret_source.contains("boundUserOpenid: qq-user-openid"));
        let runtime_source = tokio::fs::read_to_string(store.runtime_path())
            .await
            .unwrap();
        assert!(!runtime_source.contains("qq-secret"));

        let summary = manager.summary(ChannelKind::Qq).await.unwrap();
        assert!(summary.connected);
        assert_eq!(summary.bound_user_id.as_deref(), Some("qq-user-openid"));
        let capability = dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
            .into_iter()
            .find(|capability| capability.name == QQ_CHANNEL)
            .unwrap()
            .content;
        assert!(capability.contains("dwo channel qq send-file"));
        assert!(!capability.contains("qq-secret"));

        manager.remove(ChannelKind::Qq).await.unwrap();
        assert!(!store.secret_path().exists());
        assert!(!store.runtime_path().exists());
        assert!(
            dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
                .into_iter()
                .all(|capability| capability.name != QQ_CHANNEL)
        );
    }

    #[tokio::test]
    async fn feishu_requires_credential_environment_names() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
appIdEnv: ""
appSecretEnv: FEISHU_APP_SECRET
platform: feishu
mediaInput: true
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(FEISHU_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let error = ChannelManager::new(root.path(), &channels)
            .await
            .err()
            .expect("empty appIdEnv should fail");

        assert!(format!("{error:#}").contains("appIdEnv"));
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
