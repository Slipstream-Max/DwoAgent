use std::path::Path;

use anyhow::{Context, Result};
use dwo_agent_service::LoggingConfig;
use serde::Deserialize;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::host;

const LOG_FILTER_ENV: &str = "DWO_LOG";

pub struct LoggingGuard {
    _writer: WorkerGuard,
}

#[derive(Default, Deserialize)]
struct BootstrapProfile {
    #[serde(default)]
    logging: LoggingConfig,
}

pub fn init(config_path: &Path) -> Result<LoggingGuard> {
    let root = host::profile_root(config_path)?;
    let log_dir = root.join("runtime/logs");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("create log directory {}", log_dir.display()))?;

    let (config, profile_error) = load_config(&root);
    let directive =
        std::env::var(LOG_FILTER_ENV).unwrap_or_else(|_| config.level.as_str().to_string());
    let filter = EnvFilter::try_new(&directive)
        .with_context(|| format!("parse {LOG_FILTER_ENV} logging filter {directive:?}"))?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("dwo")
        .filename_suffix("jsonl")
        .max_log_files(config.retention_days)
        .build(&log_dir)
        .with_context(|| format!("open log file in {}", log_dir.display()))?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(false)
        .finish(appender);

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .with_writer(writer),
        )
        .try_init()
        .context("initialize structured logging")?;
    install_panic_hook();

    if let Some(error) = profile_error {
        tracing::warn!(
            event = "logging.profile_fallback",
            error = %error,
            "using default logging configuration"
        );
    }
    tracing::info!(
        event = "logging.initialized",
        directory = %log_dir.display(),
        filter = %directive,
        retention_days = config.retention_days,
        "structured file logging initialized"
    );

    Ok(LoggingGuard { _writer: guard })
}

fn load_config(root: &Path) -> (LoggingConfig, Option<String>) {
    let path = root.join("profile.yaml");
    match std::fs::read_to_string(&path)
        .with_context(|| format!("read logging configuration from {}", path.display()))
        .and_then(|source| {
            serde_yaml::from_str::<BootstrapProfile>(&source)
                .context("parse logging configuration from profile.yaml")
        }) {
        Ok(profile) if (1..=365).contains(&profile.logging.retention_days) => {
            (profile.logging, None)
        }
        Ok(_) => (
            LoggingConfig::default(),
            Some("logging.retentionDays must be between 1 and 365".to_string()),
        ),
        Err(error) => (LoggingConfig::default(), Some(format!("{error:#}"))),
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic| {
        let message = panic
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        let location = panic
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown".to_string());
        tracing::error!(
            event = "process.panic",
            location = %location,
            error = %message,
            "process panicked"
        );
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_profile_ignores_unrelated_fields() {
        let profile: BootstrapProfile = serde_yaml::from_str(
            r#"
name: coder
logging:
  level: warn
  retentionDays: 7
unknownFutureField: true
"#,
        )
        .unwrap();

        assert_eq!(profile.logging.level.as_str(), "warn");
        assert_eq!(profile.logging.retention_days, 7);
    }
}
