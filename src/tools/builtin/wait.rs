//! Wait tool runtime.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::tools::session::{Cap, ToolSession};

#[derive(Debug)]
pub enum WaitTarget {
    Sleep,
    Terminal(String),
    Subagent(String),
}

pub fn parse_wait_target(args: &Map<String, Value>) -> Result<(f64, WaitTarget)> {
    let seconds = args.get("seconds").and_then(Value::as_f64).unwrap_or(0.0);
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("seconds must be a positive number.");
    }

    let terminal_name = args
        .get("terminal_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let subagent_name = args
        .get("subagent_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match (terminal_name, subagent_name) {
        (Some(_), Some(_)) => bail!("Use only one of terminal_name or subagent_name."),
        (Some(name), None) => Ok((seconds, WaitTarget::Terminal(name.to_string()))),
        (None, Some(name)) => Ok((seconds, WaitTarget::Subagent(name.to_string()))),
        (None, None) => Ok((seconds, WaitTarget::Sleep)),
    }
}

pub async fn wait_seconds(seconds: f64) -> Result<Value> {
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("seconds must be a positive number.");
    }
    tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
    Ok(json!({
        "tool": "wait",
        "kind": "wait",
        "status": "completed",
        "seconds": seconds,
    }))
}

pub async fn wait_session(session: &Arc<Mutex<dyn ToolSession>>, seconds: f64) -> Result<Value> {
    let mut args = Map::new();
    args.insert("tool".to_string(), Value::String("wait".to_string()));

    let mut guard = session.lock().await;
    if !guard.capabilities().contains(&Cap::Wait) {
        bail!("session does not support wait");
    }
    guard.wait(seconds, &args).await
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;

    #[test]
    fn parse_wait_target_rejects_both_session_names() {
        let args = json!({
            "seconds": 1,
            "terminal_name": "powershell-1",
            "subagent_name": "alice",
        });
        let err = parse_wait_target(args.as_object().unwrap()).unwrap_err();

        assert!(err.to_string().contains("Use only one"));
    }

    #[tokio::test]
    async fn wait_seconds_sleeps_for_requested_time() {
        let started = Instant::now();

        let output = wait_seconds(0.1).await.unwrap();

        assert_eq!(output["tool"], "wait");
        assert_eq!(output["kind"], "wait");
        assert_eq!(output["status"], "completed");
        assert_eq!(output["seconds"], 0.1);
        assert!(started.elapsed() >= Duration::from_millis(90));
    }
}
