use std::process::Command;

#[test]
fn daemon_startup_failure_is_written_as_jsonl() {
    let profile = tempfile::tempdir().unwrap();
    let config_path = profile.path().join("profile.yaml");
    std::fs::write(
        &config_path,
        r#"
name: ""
description: invalid profile used to test logging
policyMode: confirm
logging:
  level: info
  retentionDays: 3
model:
  defaultModelId: test
  providers:
    local:
      type: local
  models:
    - modelName: test
      provider: local
      modelId: test
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dwo"))
        .args(["--config-path", config_path.to_str().unwrap(), "serve"])
        .env_remove("DWO_LOG")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let log_dir = profile.path().join("runtime/logs");
    let files = std::fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    let contents = std::fs::read_to_string(&files[0]).unwrap();
    let events = contents
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "logging.initialized")
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "daemon.starting")
    );
    assert!(events.iter().any(|event| event["event"] == "daemon.failed"));
}
