//! DeepSeek OpenAI-compatible client.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use super::base::{BaseLlmClient, ReasoningShaper};
use crate::config::models::{ModelCapabilities, ModelConfig};

/// Reasoning-mode shaper producing DeepSeek V4's `extra_body.thinking`
/// + `reasoning_effort` kwargs.
pub struct DeepSeekReasoning;

impl ReasoningShaper for DeepSeekReasoning {
    fn reasoning_kwargs(&self, reasoning_mode: &str) -> Result<Map<String, Value>> {
        let mode = reasoning_mode.trim();
        let mode = if mode.is_empty() { "auto" } else { mode };

        let mut kwargs = Map::new();
        match mode {
            "auto" => {}
            "nonthink" => {
                kwargs.insert(
                    "extra_body".to_string(),
                    json!({"thinking": {"type": "disabled"}}),
                );
            }
            "high" => {
                kwargs.insert(
                    "extra_body".to_string(),
                    json!({"thinking": {"type": "enabled"}}),
                );
                kwargs.insert(
                    "reasoning_effort".to_string(),
                    Value::String("high".to_string()),
                );
            }
            "max" => {
                kwargs.insert(
                    "extra_body".to_string(),
                    json!({"thinking": {"type": "enabled"}}),
                );
                kwargs.insert(
                    "reasoning_effort".to_string(),
                    Value::String("max".to_string()),
                );
            }
            other => bail!("Unsupported DeepSeek reasoning_mode: {other}"),
        }
        Ok(kwargs)
    }
}

pub fn new_deepseek_client(
    config: ModelConfig,
    capabilities: ModelCapabilities,
    default_reasoning_mode: impl Into<String>,
) -> Result<BaseLlmClient> {
    BaseLlmClient::new(
        config,
        capabilities,
        default_reasoning_mode,
        Box::new(DeepSeekReasoning),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_reasoning_modes_match_provider_kwargs() {
        let shaper = DeepSeekReasoning;

        assert!(shaper.reasoning_kwargs("auto").unwrap().is_empty());
        assert_eq!(
            shaper.reasoning_kwargs("nonthink").unwrap(),
            serde_json::json!({"extra_body": {"thinking": {"type": "disabled"}}})
                .as_object()
                .unwrap()
                .clone()
        );
        assert_eq!(
            shaper.reasoning_kwargs("high").unwrap(),
            serde_json::json!({
                "extra_body": {"thinking": {"type": "enabled"}},
                "reasoning_effort": "high",
            })
            .as_object()
            .unwrap()
            .clone()
        );
        assert_eq!(
            shaper.reasoning_kwargs("max").unwrap(),
            serde_json::json!({
                "extra_body": {"thinking": {"type": "enabled"}},
                "reasoning_effort": "max",
            })
            .as_object()
            .unwrap()
            .clone()
        );
    }

    #[test]
    fn deepseek_rejects_unknown_reasoning_mode() {
        let error = DeepSeekReasoning
            .reasoning_kwargs("medium")
            .unwrap_err()
            .to_string();

        assert!(error.contains("Unsupported DeepSeek reasoning_mode: medium"));
    }
}
