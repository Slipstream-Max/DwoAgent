use std::path::PathBuf;
use std::time::Duration;

use dwo_agent_service::{SessionId, SessionListItem};
use serde::{Deserialize, Serialize};

/// Interval used by CLI bind flows while polling channel binding progress.
pub const BIND_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    Weixin,
    Telegram,
    Feishu,
    Qq,
}

impl ChannelKind {
    pub const ALL: [Self; 4] = [Self::Weixin, Self::Telegram, Self::Feishu, Self::Qq];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Weixin => "weixin",
            Self::Telegram => "telegram",
            Self::Feishu => "feishu",
            Self::Qq => "qq",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Weixin => "Weixin",
            Self::Telegram => "Telegram",
            Self::Feishu => "Feishu",
            Self::Qq => "QQ",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.as_str() == value)
    }
}

#[derive(Debug, Default)]
pub struct SessionCreateRequest {
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub from: Option<SessionId>,
    pub project: Option<String>,
    pub section: Option<String>,
}

#[derive(Clone)]
pub struct ChannelSessionItem {
    pub session: SessionListItem,
    pub section: Option<String>,
    /// 会话当前有活跃回合；Inbox 视图用它分组「进行中 / 已完成」。
    pub working: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSummary {
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
    pub selected_session_id: Option<String>,
    pub bound_user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelPollParams {
    pub binding_id: String,
    pub verify_code: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TelegramBindProgress {
    Waiting,
    Confirmed { channel: ChannelSummary },
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FeishuBindProgress {
    Waiting,
    Confirmed { channel: ChannelSummary },
    Expired,
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QqBindProgress {
    Waiting,
    Confirmed { channel: ChannelSummary },
    Expired,
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChannelBindingProgress {
    Waiting,
    Scanned,
    Confirmed { channel: ChannelSummary },
    Expired,
    NeedVerifyCode,
    Failed { message: String },
}

impl From<WeixinLoginProgress> for ChannelBindingProgress {
    fn from(progress: WeixinLoginProgress) -> Self {
        match progress {
            WeixinLoginProgress::Waiting => Self::Waiting,
            WeixinLoginProgress::Scanned => Self::Scanned,
            WeixinLoginProgress::Confirmed { channel } => Self::Confirmed { channel },
            WeixinLoginProgress::Expired => Self::Expired,
            WeixinLoginProgress::NeedVerifyCode => Self::NeedVerifyCode,
            WeixinLoginProgress::Failed { message } => Self::Failed { message },
        }
    }
}

impl From<TelegramBindProgress> for ChannelBindingProgress {
    fn from(progress: TelegramBindProgress) -> Self {
        match progress {
            TelegramBindProgress::Waiting => Self::Waiting,
            TelegramBindProgress::Confirmed { channel } => Self::Confirmed { channel },
            TelegramBindProgress::Expired => Self::Expired,
        }
    }
}

impl From<FeishuBindProgress> for ChannelBindingProgress {
    fn from(progress: FeishuBindProgress) -> Self {
        match progress {
            FeishuBindProgress::Waiting => Self::Waiting,
            FeishuBindProgress::Confirmed { channel } => Self::Confirmed { channel },
            FeishuBindProgress::Expired => Self::Expired,
            FeishuBindProgress::Failed { message } => Self::Failed { message },
        }
    }
}

impl From<QqBindProgress> for ChannelBindingProgress {
    fn from(progress: QqBindProgress) -> Self {
        match progress {
            QqBindProgress::Waiting => Self::Waiting,
            QqBindProgress::Confirmed { channel } => Self::Confirmed { channel },
            QqBindProgress::Expired => Self::Expired,
            QqBindProgress::Failed { message } => Self::Failed { message },
        }
    }
}

/// 未配置渠道的默认配置：管理面永远列全四家，首次绑定 / 启用 / 改设置时
/// 由 host 把这份默认值写回 profile 的 `channels.<name>` 段。
///
/// 取值跟 `dwo init` 模板一致：开关关着、回放 5 轮；飞书用标准的两个
/// 环境变量名；telegram 代理缺省直连。
pub fn default_channel_config(name: &str) -> serde_yaml::Value {
    let source = match ChannelKind::parse(name) {
        Some(ChannelKind::Telegram) => "enabled: false\nreplayTurns: 5\ntgProxy: null\n",
        Some(ChannelKind::Feishu) => "enabled: false\nreplayTurns: 5\nplatform: feishu\n",
        _ => "enabled: false\nreplayTurns: 5\n",
    };
    serde_yaml::from_str(source).expect("default channel config is valid YAML")
}
