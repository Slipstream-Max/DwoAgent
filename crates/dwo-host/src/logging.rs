use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use dwo_agent_service::LoggingConfig;
use serde::Deserialize;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;

use crate::host;

const LOG_FILTER_ENV: &str = "DWO_LOG";

pub struct LoggingGuard {
    _writer: WorkerGuard,
    maintenance: tokio::task::JoinHandle<()>,
}

type FilterHandle = reload::Handle<EnvFilter, Registry>;

static FILTER_HANDLE: OnceLock<FilterHandle> = OnceLock::new();
static LOG_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static RETENTION_DAYS: AtomicUsize = AtomicUsize::new(14);

#[derive(Default, Deserialize)]
struct BootstrapProfile {
    #[serde(default)]
    logging: LoggingConfig,
}

pub fn init(config_path: &Path) -> Result<LoggingGuard> {
    let root = host::profile_root(config_path)?;
    let log_dir = root.join("logs");
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
        .max_log_files(365)
        .build(&log_dir)
        .with_context(|| format!("open log file in {}", log_dir.display()))?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(false)
        .finish(appender);

    let (filter, filter_handle) = reload::Layer::new(filter);
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
    let _ = FILTER_HANDLE.set(filter_handle);
    let _ = LOG_DIRECTORY.set(log_dir.clone());
    RETENTION_DAYS.store(config.retention_days, Ordering::Relaxed);
    cleanup_logs(&log_dir, config.retention_days)?;
    let maintenance_dir = log_dir.clone();
    let maintenance = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            interval.tick().await;
            let retention = RETENTION_DAYS.load(Ordering::Relaxed);
            if let Err(error) = cleanup_logs(&maintenance_dir, retention) {
                tracing::warn!(
                    event = "logging.cleanup_failed",
                    error = %format!("{error:#}"),
                    "clean up retained log files failed"
                );
            }
        }
    });
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

    Ok(LoggingGuard {
        _writer: guard,
        maintenance,
    })
}

impl Drop for LoggingGuard {
    fn drop(&mut self) {
        self.maintenance.abort();
    }
}

pub fn reload(config: &LoggingConfig) -> Result<()> {
    RETENTION_DAYS.store(config.retention_days, Ordering::Relaxed);
    if std::env::var_os(LOG_FILTER_ENV).is_none()
        && let Some(handle) = FILTER_HANDLE.get()
    {
        handle
            .reload(EnvFilter::try_new(config.level.as_str())?)
            .context("reload logging filter")?;
    }
    if let Some(directory) = LOG_DIRECTORY.get() {
        cleanup_logs(directory, config.retention_days)?;
    }
    Ok(())
}

fn cleanup_logs(directory: &Path, retention: usize) -> Result<()> {
    let mut files = std::fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            entry.path().is_file() && name.starts_with("dwo") && name.ends_with("jsonl")
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|entry| {
        std::cmp::Reverse(
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    });
    for entry in files.into_iter().skip(retention) {
        std::fs::remove_file(entry.path())?;
    }
    Ok(())
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
