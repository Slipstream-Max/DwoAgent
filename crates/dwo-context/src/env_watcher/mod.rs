mod change;
mod render;

use serde::{Deserialize, Serialize};

pub use change::EnvChange;

use crate::prompt::{
    ChannelCapabilitySnapshot, EnvironmentSnapshot, McpSnapshot, RuleSnapshot, SkillSnapshot,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicEnvironmentSnapshot {
    pub agent_prompt: String,
    pub rules: Vec<RuleSnapshot>,
    pub skills: Vec<SkillSnapshot>,
    #[serde(default)]
    pub channels: Vec<ChannelCapabilitySnapshot>,
    pub mcp: Option<McpSnapshot>,
    pub environment: EnvironmentSnapshot,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvWatcherState {
    pub baseline: Option<DynamicEnvironmentSnapshot>,
}

impl EnvWatcherState {
    pub fn new(baseline: DynamicEnvironmentSnapshot) -> Self {
        Self {
            baseline: Some(baseline),
        }
    }

    pub fn update(&mut self, current: DynamicEnvironmentSnapshot) -> Vec<EnvChange> {
        let Some(previous) = self.baseline.replace(current.clone()) else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        if previous.agent_prompt != current.agent_prompt {
            changes.push(EnvChange::AgentPrompt {
                content: current.agent_prompt.clone(),
            });
        }
        if previous.rules != current.rules {
            changes.push(EnvChange::Rules {
                rules: current.rules.clone(),
            });
        }
        if previous.skills != current.skills {
            changes.push(EnvChange::Skills {
                skills: current.skills.clone(),
            });
        }
        if previous.channels != current.channels {
            changes.push(EnvChange::Channels {
                previous: previous.channels,
                current: current.channels.clone(),
            });
        }
        if previous.mcp != current.mcp {
            changes.push(EnvChange::Mcp {
                config: current.mcp.clone(),
            });
        }
        if previous.environment != current.environment {
            changes.push(EnvChange::Environment {
                environment: current.environment,
            });
        }
        changes
    }
}
