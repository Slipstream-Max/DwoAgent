use super::*;
use dwo_agent_service::{SessionConfigUpdate, SessionListQuery};

pub(super) fn write_test_profile(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root.join("resource/prompts")).unwrap();
    std::fs::write(
        root.join("resource/prompts/System.md"),
        "You are a test agent.",
    )
    .unwrap();
    let config = root.join("profile.yaml");
    std::fs::write(
        &config,
        r#"policyMode: confirm
model:
  default:
    model: deepseek/deepseek-v4-pro
  providers:
    deepseek:
"#,
    )
    .unwrap();
    config
}

#[tokio::test]
async fn profile_yaml_reloads_all_host_configuration_atomically() {
    let root = tempfile::tempdir().unwrap();
    let config = write_test_profile(root.path());
    let host = Host::build(&config).await.unwrap();
    let existing_id = host
        .create_session(HostSessionOptions::default())
        .await
        .unwrap();
    std::fs::write(
        &config,
        r#"policyMode: watch
maxModelSteps: 17
logging:
  level: debug
  retentionDays: 7
model:
  default:
    model: deepseek/deepseek-v4-flash
  providers:
    deepseek:
websocket:
  enabled: false
  bind: 127.0.0.1
  port: 19000
"#,
    )
    .unwrap();

    assert!(host.reload_profile_if_changed().await.unwrap());
    let snapshot = host
        .handle_method("config.snapshot", json!({}))
        .await
        .unwrap();
    assert_eq!(snapshot["policy"], "watch");
    assert_eq!(snapshot["defaultModel"], "deepseek/deepseek-v4-flash");
    assert_eq!(snapshot["models"].as_array().unwrap().len(), 3);
    assert!(host.channels().list().await.unwrap().is_empty());
    host.service
        .set_config(
            &existing_id,
            SessionConfigUpdate::Model("deepseek/deepseek-v4-flash".to_string()),
        )
        .await
        .unwrap();

    let session_id = host
        .create_session(HostSessionOptions::default())
        .await
        .unwrap();
    let record = host.service.snapshot(&session_id).await.unwrap().record;
    assert_eq!(record.info.mode, SessionMode::Watch);
    assert_eq!(record.llm.model, "deepseek/deepseek-v4-flash");

    let invalid = std::fs::read_to_string(&config).unwrap().replacen(
        "policyMode: watch",
        "policyMode: invalid",
        1,
    );
    std::fs::write(&config, invalid).unwrap();
    assert!(host.reload_profile_if_changed().await.is_err());
    let snapshot = host
        .handle_method("config.snapshot", json!({}))
        .await
        .unwrap();
    assert_eq!(snapshot["defaultModel"], "deepseek/deepseek-v4-flash");

    host.shutdown().await;
}

#[tokio::test]
async fn transport_request_ids_deduplicate_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let host = Host::build(&write_test_profile(root.path())).await.unwrap();
    let first = host
        .handle_request(
            "client-a",
            "retry-1",
            "session.new",
            json!({"title": "one"}),
        )
        .await
        .unwrap();
    let second = host
        .handle_request(
            "client-a",
            "retry-1",
            "session.new",
            json!({"title": "one"}),
        )
        .await
        .unwrap();
    assert_eq!(first["session_id"], second["session_id"]);
    assert_eq!(
        host.service
            .list(SessionListQuery::new(None, None))
            .await
            .unwrap()
            .sessions
            .len(),
        1
    );

    let different_client = host
        .handle_request(
            "client-b",
            "retry-1",
            "session.new",
            json!({"title": "two"}),
        )
        .await
        .unwrap();
    assert_ne!(first["session_id"], different_client["session_id"]);

    let reused = host
        .handle_request(
            "client-a",
            "retry-1",
            "session.new",
            json!({"title": "different"}),
        )
        .await;
    assert!(reused.is_err());
    assert_eq!(
        host.service
            .list(SessionListQuery::new(None, None))
            .await
            .unwrap()
            .sessions
            .len(),
        2
    );
    host.shutdown().await;
}

#[tokio::test]
async fn transport_request_cache_stays_bounded_under_client_load() {
    let root = tempfile::tempdir().unwrap();
    let host = Host::build(&write_test_profile(root.path())).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for index in 0..2048 {
            host.handle_request(
                "load-client",
                &format!("request-{index}"),
                "daemon.shutdown",
                json!({}),
            )
            .await
            .unwrap();
        }
    })
    .await
    .expect("request cache load took more than five seconds");

    assert_eq!(host.request_cache.lock().await.len(), 1024);
    host.shutdown().await;
}
