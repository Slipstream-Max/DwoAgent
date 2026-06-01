//! Skill-related exceptions.

use thiserror::Error;

/// Base variant set for skill-related errors. `SkillError` is a sum of the
/// Python subclass hierarchy so callers can pattern-match.
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("{0}")]
    Parse(String),

    #[error("{message}")]
    Validation {
        message: String,
        errors: Vec<String>,
    },
}

impl SkillError {
    pub fn parse(msg: impl Into<String>) -> Self {
        Self::Parse(msg.into())
    }

    pub fn validation(message: impl Into<String>, errors: Vec<String>) -> Self {
        let message = message.into();
        let errors = if errors.is_empty() {
            vec![message.clone()]
        } else {
            errors
        };
        Self::Validation { message, errors }
    }
}
