use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dwo_context::ChannelCapabilitySnapshot;
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::UpdateKind;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;
use weixin_agent::{LoginStatus, QrLoginSession, StandaloneQrLogin, WeixinConfig};

use super::{
    ChannelKind, ChannelSummary, FeishuBindProgress, QqBindProgress, TelegramBindProgress,
    WeixinLoginProgress,
};

const WEIXIN_CHANNEL: &str = ChannelKind::Weixin.as_str();
const TELEGRAM_CHANNEL: &str = ChannelKind::Telegram.as_str();
const FEISHU_CHANNEL: &str = ChannelKind::Feishu.as_str();
const QQ_CHANNEL: &str = ChannelKind::Qq.as_str();
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WeixinChannelConfig {
    pub enabled: bool,
    pub replay_turns: usize,
}

impl WeixinChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.weixin.replayTurns must be at most 10");
        }
        Ok(())
    }
}

impl Default for WeixinChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            replay_turns: 5,
        }
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
    pub tg_proxy: Option<String>,
}

impl TelegramChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.telegram.replayTurns must be at most 10");
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

impl Default for TelegramChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            replay_turns: 5,
            tg_proxy: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeishuPlatform {
    Feishu,
    Lark,
}

impl FeishuPlatform {
    pub fn base_url(self) -> &'static str {
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
    pub platform: FeishuPlatform,
}

impl FeishuChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.feishu.replayTurns must be at most 10");
        }
        Ok(())
    }
}

impl Default for FeishuChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            replay_turns: 5,
            platform: FeishuPlatform::Feishu,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QqChannelConfig {
    pub enabled: bool,
    pub replay_turns: usize,
}

impl QqChannelConfig {
    fn validate(&self) -> Result<()> {
        if self.replay_turns > 10 {
            bail!("channels.qq.replayTurns must be at most 10");
        }
        Ok(())
    }
}

impl Default for QqChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            replay_turns: 5,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TelegramSecret {
    /// 向导在绑定流程里填的 bot token —— 跟微信 / QQ 对齐：凭据都住 secret.yaml。
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub bot_id: u64,
    #[serde(default)]
    pub bot_username: String,
    #[serde(default)]
    pub bound_user_id: u64,
    #[serde(default)]
    pub bound_chat_id: i64,
}

impl TelegramSecret {
    fn token(&self) -> Option<&str> {
        self.bot_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
    }

    /// 四个身份字段齐了才算绑上（向导第 1 步只写 token，不写身份）。
    fn is_bound(&self) -> bool {
        self.bot_id != 0
            && !self.bot_username.trim().is_empty()
            && self.bound_user_id != 0
            && self.bound_chat_id > 0
    }

    fn validate(&self) -> Result<()> {
        if self.bot_token.is_some() && self.token().is_none() {
            bail!("channels.telegram secret field botToken must not be empty");
        }
        let untouched = self.bot_id == 0
            && self.bot_username.trim().is_empty()
            && self.bound_user_id == 0
            && self.bound_chat_id == 0;
        if !self.is_bound() && !untouched {
            bail!(
                "channels.telegram secret must either be a complete binding \
                 or hold only the bot token"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeishuSecret {
    /// 向导第 2 步填的 App ID / Secret —— 跟微信 / QQ / Telegram 对齐：凭据都住 secret.yaml。
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
    #[serde(default)]
    pub bound_open_id: String,
    #[serde(default)]
    pub bound_chat_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QqSecret {
    pub app_id: String,
    pub app_secret: String,
    pub bound_user_openid: String,
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
    fn credentials(&self) -> Option<(String, String)> {
        let app_id = self
            .app_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())?;
        let app_secret = self
            .app_secret
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())?;
        Some((app_id.to_string(), app_secret.to_string()))
    }

    /// open_id + chat_id 齐了才算绑上（向导第 2 步只写凭据，不写身份）。
    fn is_bound(&self) -> bool {
        !self.bound_open_id.trim().is_empty() && !self.bound_chat_id.trim().is_empty()
    }

    fn validate(&self) -> Result<()> {
        if self
            .app_id
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            bail!("channels.feishu secret field appId must not be empty");
        }
        if self
            .app_secret
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            bail!("channels.feishu secret field appSecret must not be empty");
        }
        if self.app_id.is_some() != self.app_secret.is_some() {
            bail!("channels.feishu secret must provide both appId and appSecret");
        }
        if self.is_bound() {
            return Ok(());
        }
        let untouched =
            self.bound_open_id.trim().is_empty() && self.bound_chat_id.trim().is_empty();
        if !untouched {
            bail!("channels.feishu secret must have both boundOpenId and boundChatId, or neither");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WeixinSecret {
    pub bot_token: String,
    pub base_url: String,
    pub ilink_bot_id: String,
    pub bound_user_id: String,
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

pub struct WeixinRuntime {
    pub config: WeixinChannelConfig,
    pub state: WeixinChannelState,
    pub secret: WeixinSecret,
}

pub struct TelegramRuntime {
    pub config: TelegramChannelConfig,
    pub state: TelegramChannelState,
    pub secret: TelegramSecret,
    pub bot_token: String,
}

pub struct FeishuRuntime {
    pub config: FeishuChannelConfig,
    pub state: FeishuChannelState,
    pub secret: FeishuSecret,
    pub app_id: String,
    pub app_secret: String,
}

pub struct QqRuntime {
    pub config: QqChannelConfig,
    pub state: QqChannelState,
    pub secret: QqSecret,
    pub app_id: String,
    pub app_secret: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WeixinLoginStart {
    pub binding_id: String,
    pub qrcode: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramBindStart {
    pub binding_id: String,
    pub code: String,
    pub bot_username: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeishuBindStart {
    pub binding_id: String,
    pub code: String,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QqBindStart {
    pub binding_id: String,
    pub qrcode: String,
}

/// 拉一枪微信二维码 —— 上游这枪**没有超时兜底**（`weixin-agent` 的
/// `api_post` 传的是 `timeout=None`，连接一进黑洞就会无限挂）。这儿包一层：
/// 每枪 15s，失败再补一枪，两枪都不行就把错误抛回去。
async fn request_weixin_qrcode(
    login: &StandaloneQrLogin,
    existing: &[String],
) -> Result<QrLoginSession> {
    const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=2 {
        match tokio::time::timeout(ATTEMPT_TIMEOUT, login.start(None, existing)).await {
            Ok(Ok(session)) => return Ok(session),
            Ok(Err(error)) => {
                tracing::warn!(attempt, error = %error, "weixin qrcode request failed");
                last_error = Some(error.into());
            }
            Err(_) => {
                tracing::warn!(attempt, "weixin qrcode request timed out");
                last_error = Some(anyhow::anyhow!("weixin qrcode request timed out"));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("weixin qrcode request failed")))
}

struct PendingLogin {
    session: QrLoginSession,
    config: WeixinConfig,
    created_at: Instant,
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

    /// 读 secret（不存在就交一份默认值）——telegram 的向导第 1 步会先只写 token。
    async fn load_secret_or_default<T: serde::de::DeserializeOwned + Default>(&self) -> Result<T> {
        read_yaml_or_default(&self.secret_path()).await
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
                WEIXIN_CHANNEL | TELEGRAM_CHANNEL | FEISHU_CHANNEL | QQ_CHANNEL
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
        let root = profile_root.join("channels");
        tokio::fs::create_dir_all(&root).await?;
        let manager = Self {
            profile_root: profile_root.to_path_buf(),
            root,
            weixin,
            telegram,
            feishu,
            qq,
            pending_weixin: Mutex::new(HashMap::new()),
            pending_telegram: Mutex::new(HashMap::new()),
            pending_feishu: Mutex::new(HashMap::new()),
            pending_qq: Mutex::new(HashMap::new()),
            capabilities: Mutex::new(BTreeMap::new()),
        };
        manager.sync_capabilities().await?;
        Ok(manager)
    }

    /// profile 不变量：`enabled: true` 只允许出现在**绑定过**的渠道上。
    ///
    /// 三条写入路径已经把门守住了（`channel_set_enabled` 要求有绑定、
    /// 解绑 / 移除会自动关开关、配置写回也不许塞 `enabled=true`）——
    /// 这儿兜手工改 profile.yaml 的底：不合法就直接报错，别带病启动。
    pub(crate) fn validate_enabled_bindings(&self) -> Result<()> {
        for (name, enabled) in [
            (WEIXIN_CHANNEL, self.weixin.as_ref().map(|config| config.enabled)),
            (
                TELEGRAM_CHANNEL,
                self.telegram.as_ref().map(|config| config.enabled),
            ),
            (FEISHU_CHANNEL, self.feishu.as_ref().map(|config| config.enabled)),
            (QQ_CHANNEL, self.qq.as_ref().map(|config| config.enabled)),
        ] {
            if enabled != Some(true) {
                continue;
            }
            let secret_path = self.root.join(name).join("secret.yaml");
            if !secret_path.is_file() {
                bail!(
                    "channels.{name}: enabled=true without a bound account \
                     (bind it first, or set enabled=false)"
                );
            }
            if name == TELEGRAM_CHANNEL {
                // Telegram 光绑上还不够：向导第 1 步存的 token 也得在，运行时才拿得动。
                let source = std::fs::read_to_string(&secret_path)
                    .with_context(|| format!("read {}", secret_path.display()))?;
                let secret: TelegramSecret = serde_yaml::from_str(&source)
                    .with_context(|| format!("parse {}", secret_path.display()))?;
                secret.validate()?;
                anyhow::ensure!(
                    secret.is_bound() && secret.token().is_some(),
                    "channels.{name}: enabled=true without a bound account and a saved bot \
                     token (bind it first, or set enabled=false)"
                );
            }
            if name == FEISHU_CHANNEL {
                // 飞书同理：绑定齐 + 凭据都在（向导两步都走完）才能真的跑起来。
                let source = std::fs::read_to_string(&secret_path)
                    .with_context(|| format!("read {}", secret_path.display()))?;
                let secret: FeishuSecret = serde_yaml::from_str(&source)
                    .with_context(|| format!("parse {}", secret_path.display()))?;
                secret.validate()?;
                anyhow::ensure!(
                    secret.is_bound() && secret.credentials().is_some(),
                    "channels.{name}: enabled=true without a bound account and saved credentials \
                     (finish the bind wizard first, or set enabled=false)"
                );
            }
        }
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<ChannelSummary>> {
        // 管理面永远列全四家：没写进 profile 的渠道也汇报（默认态），
        // 桌面端才能从零开始绑定 / 启用 / 改配置。
        let mut summaries = Vec::with_capacity(ChannelKind::ALL.len());
        for channel in ChannelKind::ALL {
            summaries.push(self.summary(channel).await?);
        }
        Ok(summaries)
    }

    pub async fn summary(&self, channel: ChannelKind) -> Result<ChannelSummary> {
        let store = self.store(channel);
        match channel {
            ChannelKind::Weixin => {
                // 未配置也算一条：开关按 false 汇报。
                let enabled = self.weixin.as_ref().is_some_and(|config| config.enabled);
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
                    enabled,
                    connected,
                    selected_session_id: state.selected_session_id,
                    bound_user_id,
                })
            }
            ChannelKind::Telegram => {
                let enabled = self.telegram.as_ref().is_some_and(|config| config.enabled);
                let state: TelegramChannelState = store.load_runtime().await?;
                let (connected, bound_user_id) = if store.secret_path().is_file() {
                    let secret = store.load_secret::<TelegramSecret>().await?;
                    secret.validate()?;
                    if secret.is_bound() {
                        (secret.token().is_some(), Some(secret.bound_user_id.to_string()))
                    } else {
                        // 向导第 1 步只写了 token，还没绑定。
                        (false, None)
                    }
                } else {
                    (false, None)
                };
                Ok(ChannelSummary {
                    name: TELEGRAM_CHANNEL.to_string(),
                    enabled,
                    connected,
                    selected_session_id: state.selected_session_id,
                    bound_user_id,
                })
            }
            ChannelKind::Feishu => {
                let enabled = self.feishu.as_ref().is_some_and(|config| config.enabled);
                let state: FeishuChannelState = store.load_runtime().await?;
                let (connected, bound_user_id) = if store.secret_path().is_file() {
                    let secret = store.load_secret::<FeishuSecret>().await?;
                    secret.validate()?;
                    if secret.is_bound() {
                        (true, Some(secret.bound_open_id))
                    } else {
                        // 向导第 2 步只写了凭据，还没绑定。
                        (false, None)
                    }
                } else {
                    (false, None)
                };
                Ok(ChannelSummary {
                    name: FEISHU_CHANNEL.to_string(),
                    enabled,
                    connected,
                    selected_session_id: state.selected_session_id,
                    bound_user_id,
                })
            }
            ChannelKind::Qq => {
                let enabled = self.qq.as_ref().is_some_and(|config| config.enabled);
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
                    enabled,
                    connected,
                    selected_session_id: state.selected_session_id,
                    bound_user_id,
                })
            }
        }
    }

    pub async fn bound_target(&self, channel: ChannelKind) -> Result<String> {
        match channel {
            ChannelKind::Weixin => Ok(self.load_weixin().await?.secret.bound_user_id),
            ChannelKind::Telegram => {
                Ok(self.load_telegram().await?.secret.bound_chat_id.to_string())
            }
            ChannelKind::Feishu => Ok(self.load_feishu().await?.secret.bound_chat_id),
            ChannelKind::Qq => Ok(self.load_qq().await?.secret.bound_user_openid),
        }
    }

    pub async fn remove(&self, channel: ChannelKind) -> Result<bool> {
        let removed = self.store(channel).remove().await?;
        self.sync_channel_capability(channel).await?;
        Ok(removed)
    }

    pub async fn load_weixin(&self) -> Result<WeixinRuntime> {
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

    pub async fn save_state(&self, channel: ChannelKind, state: &impl Serialize) -> Result<()> {
        self.store(channel).save_runtime(state).await
    }

    /// 向导读 telegram 的 bot token —— 住在渠道目录的 `secret.yaml` 里
    /// （跟微信 / QQ 一致；没存过就报 `is not set`，向导据此分流回第 1 步）。
    pub async fn load_telegram_token(&self) -> Result<String> {
        let secret: TelegramSecret = self
            .store(ChannelKind::Telegram)
            .load_secret_or_default()
            .await?;
        secret
            .token()
            .map(str::to_string)
            .context("telegram bot token is not set")
    }

    /// 向导存 telegram 的 bot token（绑定流程第一步；写进 secret.yaml，不进 profile）。
    pub async fn save_telegram_token(&self, token: &str) -> Result<()> {
        anyhow::ensure!(!token.trim().is_empty(), "bot token must not be empty");
        let store = self.store(ChannelKind::Telegram);
        let mut secret: TelegramSecret = store.load_secret_or_default().await?;
        secret.bot_token = Some(token.to_string());
        secret.validate()?;
        store.save_secret(&secret).await
    }

    /// 绑定向导第 2 步：把飞书的 App ID / Secret 存进渠道目录（跟别的家同屋）。
    pub async fn save_feishu_credentials(&self, app_id: &str, app_secret: &str) -> Result<()> {
        anyhow::ensure!(
            !app_id.trim().is_empty() && !app_secret.trim().is_empty(),
            "app id / secret must not be empty"
        );
        let store = self.store(ChannelKind::Feishu);
        let mut secret: FeishuSecret = store.load_secret_or_default().await?;
        secret.app_id = Some(app_id.to_string());
        secret.app_secret = Some(app_secret.to_string());
        secret.validate()?;
        store.save_secret(&secret).await
    }

    /// 向导 / 运行时读飞书的凭据 —— 住在渠道目录的 `secret.yaml` 里。
    async fn load_feishu_credentials(&self) -> Result<(String, String)> {
        let secret: FeishuSecret = self
            .store(ChannelKind::Feishu)
            .load_secret_or_default()
            .await?;
        secret
            .credentials()
            .context("feishu App ID / Secret are not set — 在飞书绑定向导第 2 步填入后再试")
    }

    pub async fn load_telegram(&self) -> Result<TelegramRuntime> {
        let config = self
            .telegram
            .clone()
            .context("channels.telegram is not configured")?;
        let store = self.store(ChannelKind::Telegram);
        let secret: TelegramSecret = store.load_secret().await?;
        secret.validate()?;
        anyhow::ensure!(secret.is_bound(), "channels.telegram is not bound yet");
        let bot_token = secret
            .token()
            .context("telegram bot token is not set")?
            .to_string();
        Ok(TelegramRuntime {
            state: store.load_runtime().await?,
            config,
            secret,
            bot_token,
        })
    }

    pub async fn load_feishu(&self) -> Result<FeishuRuntime> {
        let config = self
            .feishu
            .clone()
            .context("channels.feishu is not configured")?;
        let store = self.store(ChannelKind::Feishu);
        let secret: FeishuSecret = store.load_secret().await?;
        secret.validate()?;
        anyhow::ensure!(secret.is_bound(), "channels.feishu is not bound yet");
        let (app_id, app_secret) = secret
            .credentials()
            .context("feishu App ID / Secret are not set")?;
        Ok(FeishuRuntime {
            state: store.load_runtime().await?,
            config,
            secret,
            app_id,
            app_secret,
        })
    }

    pub async fn load_qq(&self) -> Result<QqRuntime> {
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

    pub async fn begin_telegram_bind(&self) -> Result<TelegramBindStart> {
        // 未配置的渠道也能从管理面直接走绑定：配置缺省按默认值来。
        let config = self.telegram.clone().unwrap_or_default();
        let proxy = config.tg_proxy.as_deref();
        let token = self.load_telegram_token().await?;
        let bot = telegram_probe_bot(&token, proxy)?;
        let me = bot.get_me().await.map_err(|error| {
            anyhow::anyhow!(
                "{}",
                telegram_connect_hint(&anyhow::Error::new(error), proxy)
            )
        })?;
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
        // token（向导第 1 步存的）和身份字段合成一份完整 secret。
        let bot_token = self.load_telegram_token().await?;
        self.save_telegram(TelegramSecret {
            bot_token: Some(bot_token),
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
        // 未配置也能直接绑定：平台按默认来（向导第 1 步会写回）；凭据读 secret.yaml。
        let settings = self.feishu.clone().unwrap_or_default();
        let (app_id, app_secret) = self.load_feishu_credentials().await?;
        let config = super::feishu::openlark_config(&settings, app_id, app_secret);
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
            // 身份字段接在向导第 2 步存的凭据上（同一份 secret.yaml）。
            let mut secret: FeishuSecret = self
                .store(ChannelKind::Feishu)
                .load_secret_or_default()
                .await?;
            secret.bound_open_id = bound_open_id.clone();
            secret.bound_chat_id = bound_chat_id;
            self.save_feishu(secret).await?;
            // 身份已经落盘：回执发送失败（网络抖动这类）只记日志，别把整单绑定
            // 判死；正常路径下回执会真的发出去 —— provider 在 `openlark_config`
            // 里挂好（2026-10-10 实测：缺 provider 时这里一挂，界面就只剩红字）。
            if let Err(error) = super::feishu::send_text_to(
                &binding.config,
                &bound_open_id,
                "Feishu channel bound to this private chat",
            )
            .await
            {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "feishu bound, but the confirmation message failed"
                );
            }
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
        let config = WeixinConfig::builder()
            .token("")
            .markdown_filter(false)
            .build()?;
        let login = StandaloneQrLogin::new(&config);
        let existing = self.existing_weixin_tokens().await?;
        self.prune_pending_weixin().await;
        let session = request_weixin_qrcode(&login, &existing).await?;
        let binding_id = format!("binding-{}", Uuid::new_v4());
        let qrcode = session.qrcode_img_content.clone();
        self.pending_weixin.lock().await.insert(
            binding_id.clone(),
            Arc::new(Mutex::new(PendingLogin {
                session,
                config,
                created_at: Instant::now(),
            })),
        );
        Ok(WeixinLoginStart { binding_id, qrcode })
    }

    /// 取消（或一直没人 poll）之后留下的登录会话，超过 QR 该活的时间就收掉 ——
    /// 别在 `pending_weixin` 里攒僵尸。
    async fn prune_pending_weixin(&self) {
        const MAX_AGE: Duration = Duration::from_secs(15 * 60);
        let now = Instant::now();
        let mut pending = self.pending_weixin.lock().await;
        pending.retain(|_, slot| match slot.try_lock() {
            // 正被 poll 的别动。
            Err(_) => true,
            Ok(login) => now.saturating_duration_since(login.created_at) < MAX_AGE,
        });
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
                    if self.feishu.as_ref().is_some_and(|config| config.enabled) {
                        store
                            .load_secret::<FeishuSecret>()
                            .await
                            .is_ok_and(|secret| {
                                secret.validate().is_ok()
                                    && secret.is_bound()
                                    && secret.credentials().is_some()
                            })
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

pub fn telegram_bot(token: &str, proxy: Option<&str>) -> Result<Bot> {
    Ok(Bot::with_client(
        token.to_string(),
        telegram_client(proxy, false)?,
    ))
}

/// 绑定向导专用的客户端：带连接/总超时，保证「测试连接」几秒内必然有结论。
/// 运行时长轮询用的 `telegram_bot` 不能带总超时（会掐断 getUpdates）。
fn telegram_probe_bot(token: &str, proxy: Option<&str>) -> Result<Bot> {
    Ok(Bot::with_client(
        token.to_string(),
        telegram_client(proxy, true)?,
    ))
}

fn telegram_client(proxy: Option<&str>, probe: bool) -> Result<reqwest::Client> {
    let mut client = reqwest::Client::builder();
    if probe {
        client = client
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(12));
    }
    // 空串按「没配代理」处理：直连。
    if let Some(proxy) = proxy.filter(|proxy| !proxy.trim().is_empty()) {
        client = client.proxy(
            reqwest::Proxy::all(proxy)
                .with_context(|| format!("configure Telegram HTTP proxy {proxy}"))?,
        );
    }
    Ok(client.build()?)
}

/// 把原始网络错误翻成向导里能直接展示的人话。
fn telegram_connect_hint(error: &anyhow::Error, proxy: Option<&str>) -> String {
    let detail = format!("{error:#}");
    let lowered = detail.to_ascii_lowercase();
    let proxy_note = match proxy {
        Some(proxy) => format!("当前代理 {proxy}"),
        None => "当前未设置代理（直连）".to_string(),
    };
    if lowered.contains("invalid token")
        || lowered.contains("invalidtoken")
        || lowered.contains("unauthorized")
        || lowered.contains("401")
    {
        return "Bot Token 无效：请和 @BotFather 里给的 token 完整核对一遍。".to_string();
    }
    if lowered.contains("refused") || lowered.contains("10061") {
        return format!(
            "{proxy_note}：连接被拒绝——检查代理软件是否开着、地址端口是否填对；\
             如果本来就是直连，说明当前网络直连不通，需要配代理。"
        );
    }
    if lowered.contains("timed out") || lowered.contains("timeout") {
        return format!("{proxy_note}：连接超时——网络到 Telegram 不通，检查代理后重试。");
    }
    if lowered.contains("dns") || lowered.contains("lookup") || lowered.contains("resolve") {
        return format!("{proxy_note}：域名解析失败——直连很容易这样，建议配置代理。");
    }
    if lowered.contains("tls")
        || lowered.contains("handshake")
        || lowered.contains("eof")
        || lowered.contains("connection closed")
    {
        return format!("{proxy_note}：连接被重置（TLS 握手失败）——通常需要代理。");
    }
    format!("{proxy_note}：连接失败——{detail}")
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

#[async_trait::async_trait]
impl dwo_channel_api::ChannelControl for ChannelManager {
    async fn list(&self) -> Result<Vec<ChannelSummary>> {
        ChannelManager::list(self).await
    }

    async fn summary(&self, channel: ChannelKind) -> Result<ChannelSummary> {
        ChannelManager::summary(self, channel).await
    }

    async fn bound_target(&self, channel: ChannelKind) -> Result<String> {
        ChannelManager::bound_target(self, channel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_mode_field_is_no_longer_accepted() {
        let with_mode = serde_yaml::from_str::<QqChannelConfig>(
            "enabled: true\nreplayTurns: 1\noutputMode: final\n",
        );
        assert!(with_mode.is_err(), "outputMode must no longer be accepted");

        let old = serde_yaml::from_str::<QqChannelConfig>(
            "enabled: true\nreplayTurns: 1\nreplayMode: full\n",
        );
        assert!(old.is_err(), "replayMode must not be accepted");
    }

    #[tokio::test]
    async fn profile_settings_are_validated_before_channel_state_is_loaded() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(WEIXIN_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let summary = manager.list().await.unwrap();
        assert_eq!(summary.len(), 4, "管理面永远列全四家");
        let weixin = summary
            .iter()
            .find(|channel| channel.name == WEIXIN_CHANNEL)
            .unwrap();
        assert!(weixin.enabled);
        assert!(!weixin.connected);
        assert_eq!(manager.weixin.as_ref().unwrap().replay_turns, 5);
    }

    #[tokio::test]
    async fn channels_are_listed_even_when_unconfigured() {
        let root = tempfile::tempdir().unwrap();
        let channels = BTreeMap::new();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let summaries = manager.list().await.unwrap();
        assert_eq!(summaries.len(), 4, "管理面永远列全四家");
        for summary in &summaries {
            assert!(!summary.enabled, "{} 未配置时不该是启用态", summary.name);
            assert!(!summary.connected);
            assert!(summary.bound_user_id.is_none());
        }

        // 默认配置（首次启用 / 改设置时写回 profile 的那份）。
        let feishu = dwo_channel_api::default_channel_config(FEISHU_CHANNEL);
        assert_eq!(
            feishu.get("platform").and_then(|value| value.as_str()),
            Some("feishu")
        );
        assert!(feishu.get("appIdEnv").is_none(), "凭据字段不进配置");

        // 未配置的渠道也能直接进绑定流程：按默认配置来（telegram 缺 token 先报
        // 「is not set」，而不是「not configured」）。
        let error = manager.begin_telegram_bind().await.unwrap_err();
        assert!(format!("{error:#}").contains("is not set"), "{error:#}");
    }

    #[tokio::test]
    async fn enabled_requires_a_bound_secret_in_the_profile() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(WEIXIN_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let error = manager
            .validate_enabled_bindings()
            .err()
            .expect("unbound enabled=true must be rejected");
        assert!(format!("{error:#}").contains("enabled=true without a bound account"));

        let secret_dir = root.path().join("channels").join(WEIXIN_CHANNEL);
        tokio::fs::create_dir_all(&secret_dir).await.unwrap();
        tokio::fs::write(secret_dir.join("secret.yaml"), "bound_user_id: user\n")
            .await
            .unwrap();
        manager.validate_enabled_bindings().unwrap();
    }

    #[tokio::test]
    async fn telegram_token_lives_in_the_channel_secret_not_the_profile() {
        let root = tempfile::tempdir().unwrap();
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: false
replayTurns: 5
tgProxy: null
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(TELEGRAM_CHANNEL.to_string(), profile)]);
        // token 不在 profile 里：配置里没有它的任何字段，照样能启动。
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let missing = manager.load_telegram_token().await.unwrap_err();
        assert!(
            format!("{missing:#}").contains("is not set"),
            "向导靠这个标记分流回第 1 步"
        );

        manager.save_telegram_token("123:abc").await.unwrap();
        assert_eq!(manager.load_telegram_token().await.unwrap(), "123:abc");
        // 跟微信 / QQ 对齐：token 就住渠道目录的 secret.yaml。
        let stored = tokio::fs::read_to_string(
            root.path()
                .join("channels")
                .join("telegram")
                .join("secret.yaml"),
        )
        .await
        .unwrap();
        assert!(stored.contains("123:abc"));

        // 解绑 / 移除会连凭据一起清掉（对齐微信 / QQ：重绑重新填）。
        manager.store(ChannelKind::Telegram).remove().await.unwrap();
        assert!(manager.load_telegram_token().await.is_err());
    }

    #[tokio::test]
    async fn unknown_weixin_setting_is_rejected() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
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
tgProxy: http://127.0.0.1:7890
"#,
        )
        .unwrap();
        let channels = BTreeMap::from([(TELEGRAM_CHANNEL.to_string(), profile)]);
        let root = tempfile::tempdir().unwrap();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let summary = manager.list().await.unwrap();
        let telegram = summary
            .iter()
            .find(|channel| channel.name == TELEGRAM_CHANNEL)
            .unwrap();
        assert!(!telegram.connected);
        assert!(telegram.bound_user_id.is_none());

        let state = TelegramChannelState {
            selected_session_id: Some("session-test".to_string()),
            ..Default::default()
        };
        manager
            .save_state(ChannelKind::Telegram, &state)
            .await
            .unwrap();
        // 向导第 1 步：token 先写进 secret.yaml（对齐微信 / QQ：凭据住渠道目录）。
        manager.save_telegram_token("123:abc").await.unwrap();
        assert!(
            manager.validate_enabled_bindings().is_err(),
            "只有 token、还没绑上，enabled=true 的守卫应当拦住"
        );
        // 绑定确认：token 与四个身份字段合成一份完整 secret。
        manager
            .save_telegram(TelegramSecret {
                bot_token: Some("123:abc".to_string()),
                bot_id: 7,
                bot_username: "dwo_test_bot".to_string(),
                bound_user_id: 12345,
                bound_chat_id: 12345,
            })
            .await
            .unwrap();
        manager.validate_enabled_bindings().unwrap();
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
        assert!(secret_source.contains("botToken"));
        assert!(secret_source.contains("123:abc"));
        let summary = manager.list().await.unwrap();
        let telegram = summary
            .iter()
            .find(|channel| channel.name == TELEGRAM_CHANNEL)
            .unwrap();
        assert_eq!(telegram.bound_user_id.as_deref(), Some("12345"));
        assert!(telegram.connected, "token 和绑定都在 secret.yaml 里");
        let capability = dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
            .into_iter()
            .find(|capability| capability.name == TELEGRAM_CHANNEL)
            .unwrap()
            .content;
        assert!(capability.contains("dwo channel telegram send-file"));
        assert!(!capability.contains("123:abc"), "凭据不进能力提示");
        manager.remove(ChannelKind::Telegram).await.unwrap();
        assert!(manager.load_telegram_token().await.is_err());
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
tgProxy: ""
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
    async fn feishu_private_identity_and_session_state_are_persisted_separately() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
platform: lark
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
                app_id: Some("cli_test".to_string()),
                app_secret: Some("secret_test".to_string()),
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
        assert!(secret_source.contains("appSecret: secret_test"));
        let summary = manager.list().await.unwrap();
        let feishu = summary
            .iter()
            .find(|channel| channel.name == FEISHU_CHANNEL)
            .unwrap();
        assert_eq!(feishu.bound_user_id.as_deref(), Some("ou_user"));
        assert!(feishu.connected, "凭据和绑定都在 secret 里");
        let capability = dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
            .into_iter()
            .find(|capability| capability.name == FEISHU_CHANNEL)
            .expect("绑定齐 + 凭据在的飞书渠道应当露出能力");
        assert!(capability.content.contains("dwo channel feishu send-file"));
        assert!(!capability.content.contains("secret_test"), "凭据不进能力提示");

        manager.remove(ChannelKind::Feishu).await.unwrap();
        assert!(!store.secret_path().exists());
        assert!(!store.runtime_path().exists());
        assert!(
            dwo_context::ChannelCapabilitySnapshot::runtime(&manager.profile_root)
                .into_iter()
                .all(|capability| capability.name != FEISHU_CHANNEL)
        );
    }

    #[tokio::test]
    async fn feishu_credentials_live_in_the_secret() {
        let root = tempfile::tempdir().unwrap();
        let channels = BTreeMap::new();
        let manager = ChannelManager::new(root.path(), &channels).await.unwrap();

        let missing = manager.load_feishu_credentials().await.unwrap_err();
        assert!(
            format!("{missing:#}").contains("are not set"),
            "向导靠这个标记分流回第 2 步"
        );

        // 向导第 2 步：凭据进 secret.yaml（跟微信 / QQ / Telegram 同屋）。
        manager
            .save_feishu_credentials("cli_test", "secret_test")
            .await
            .unwrap();
        let secret: FeishuSecret = manager
            .store(ChannelKind::Feishu)
            .load_secret()
            .await
            .unwrap();
        assert_eq!(secret.app_id.as_deref(), Some("cli_test"));
        assert!(secret.credentials().is_some());
        assert!(!secret.is_bound());

        // 重存覆盖旧值（重新走一遍向导第 2 步）。
        manager
            .save_feishu_credentials("cli_next", "secret_next")
            .await
            .unwrap();
        assert_eq!(
            manager.load_feishu_credentials().await.unwrap().0,
            "cli_next"
        );
    }

    #[tokio::test]
    async fn qq_qr_secret_and_session_state_are_persisted_separately() {
        let profile = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
enabled: true
replayTurns: 5
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
