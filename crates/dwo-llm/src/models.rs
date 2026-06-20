use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$var_meta:meta])*
                $variant:ident => $literal:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $( $(#[$var_meta])* $variant ),+
        }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $( Self::$variant => $literal ),+
                }
            }

            pub fn from_str(value: &str) -> Result<Self> {
                match value {
                    $( $literal => Ok(Self::$variant), )+
                    other => anyhow::bail!(concat!(
                        "invalid ",
                        stringify!($name),
                        ": {}"
                    ), other),
                }
            }

            pub const ALL: &'static [Self] = &[ $( Self::$variant ),+ ];
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let raw = <String as Deserialize>::deserialize(d)?;
                Self::from_str(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

str_enum! {
    pub enum ReasoningMode {
        Auto => "auto",
        NoThink => "nonthink",
        High => "high",
        Max => "max",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub provider: String,
    pub model_id: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub timeout_seconds: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl ModelConfig {
    pub fn validate(&mut self) -> Result<()> {
        self.provider = normalize_required_str(&self.provider, "provider")?;
        self.model_id = normalize_required_str(&self.model_id, "model_id")?;
        self.api_key_env = normalize_optional_str(self.api_key_env.as_deref(), "api_key_env")?;
        self.api_base = normalize_optional_str(self.api_base.as_deref(), "api_base")?;
        if let Some(v) = self.max_tokens
            && v == 0
        {
            bail!("max_tokens must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub tool_use: bool,
}

fn normalize_required_str(value: &str, field: &str) -> Result<String> {
    let normalized = value.trim().to_string();
    if normalized.is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(normalized)
}

fn normalize_optional_str(value: Option<&str>, field: &str) -> Result<Option<String>> {
    match value {
        Some(value) => Ok(Some(normalize_required_str(value, field)?)),
        None => Ok(None),
    }
}
