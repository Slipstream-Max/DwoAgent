use serde::{Deserialize, Serialize};

use crate::prompt::{
    ChannelCapabilitySnapshot, EnvironmentSnapshot, McpSnapshot, RuleSnapshot, SkillSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvChange {
    AgentPrompt {
        content: String,
    },
    Rules {
        rules: Vec<RuleSnapshot>,
    },
    Skills {
        skills: Vec<SkillSnapshot>,
    },
    Channels {
        previous: Vec<ChannelCapabilitySnapshot>,
        current: Vec<ChannelCapabilitySnapshot>,
    },
    Mcp {
        config: Option<McpSnapshot>,
    },
    Environment {
        environment: EnvironmentSnapshot,
    },
}

impl EnvChange {
    pub fn render(&self) -> String {
        super::render::render(self)
    }
}
