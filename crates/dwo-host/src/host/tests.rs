use super::*;
use dwo_agent_service::SessionListQuery;

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
async fn model_option_reload_notifies_existing_session_subscribers() {
    let root = tempfile::tempdir().unwrap();
    let config = write_test_profile(root.path());
    let host = Host::build(&config).await.unwrap();
    let id = host
        .create_session(HostSessionOptions::default())
        .await
        .unwrap();
    let mut subscription = host.subscribe_session(&id, None).await.unwrap();
    let original = std::fs::read_to_string(&config).unwrap();
    let changed = format!(
        "{original}      models:\n        Renamed Pro:\n          modelId: deepseek-v4-pro\n          profile: deepseek/deepseek-v4-pro\n          reasoningEfforts: [low, high]\n          defaultReasoningEffort: low\n"
    );
    std::fs::write(&config, &changed).unwrap();
    host.reload_profile_if_changed().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = subscription.events.recv().await.unwrap();
            if matches!(
                event.payload,
                dwo_agent_service::SessionEventPayload::ConfigChanged { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let options = host
        .handle_method("session.options", json!({"session_id": id}))
        .await
        .unwrap();
    assert_eq!(options["models"][0]["name"], "Renamed Pro");
    assert_eq!(options["models"][0]["defaultReasoning"], "low");
    assert_eq!(
        options["models"][0]["reasoning"].as_array().unwrap().len(),
        2
    );
    assert_eq!(options["config"]["model"], "deepseek/deepseek-v4-pro");

    // A policy-only edit must not push unchanged model/reasoning options.
    std::fs::write(
        &config,
        changed.replace("policyMode: confirm", "policyMode: watch"),
    )
    .unwrap();
    host.reload_profile_if_changed().await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            subscription.events.recv()
        )
        .await
        .is_err()
    );
    // Changes limited to reasoning or the request ID must also trigger a push.
    for updated in [
        changed.replace("[low, high]", "[low, high, max]"),
        changed.replace(
            "defaultReasoningEffort: low",
            "defaultReasoningEffort: high",
        ),
        changed
            .replace("modelId: deepseek-v4-pro", "modelId: replacement")
            .replace(
                "model: deepseek/deepseek-v4-pro",
                "model: deepseek/replacement",
            ),
    ] {
        std::fs::write(&config, updated).unwrap();
        host.reload_profile_if_changed().await.unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            subscription.events.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            event.payload,
            dwo_agent_service::SessionEventPayload::ConfigChanged { .. }
        ));
    }
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

#[tokio::test]
async fn runtime_state_files_stay_locked_while_the_host_runs() {
    let root = tempfile::tempdir().unwrap();
    // Guards are keyed by the host's canonical paths, not the tempdir spelling.
    let profile_root = std::fs::canonicalize(root.path()).unwrap();
    let workspace = profile_root.join("locked-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let host = Host::build(&write_test_profile(&profile_root))
        .await
        .unwrap();
    let locked = dwo_file_guard::capability() == dwo_file_guard::Capability::DenyWrite;

    let session_id = host
        .create_session(HostSessionOptions {
            title: Some("locked".to_string()),
            ..HostSessionOptions::default()
        })
        .await
        .unwrap();
    let session_file =
        find_session_directory(&profile_root, session_id.as_str()).join("session.json");
    if locked {
        assert!(dwo_file_guard::is_protected(&session_file));
        assert!(std::fs::write(&session_file, b"{}").is_err());
        assert!(std::fs::remove_file(&session_file).is_err());
    }

    host.handle_method(
        "session.set",
        json!({"session_id": session_id, "title": "renamed"}),
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(&session_file)
            .unwrap()
            .contains("renamed")
    );

    let project = host
        .handle_method("project.create", json!({"pwd": workspace}))
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap().to_string();
    let project_file = profile_root
        .join("runtime/projects")
        .join(&project_id)
        .join("project.json");
    if locked {
        assert!(dwo_file_guard::is_protected(&project_file));
        assert!(std::fs::write(&project_file, b"{}").is_err());
    }
    host.handle_method(
        "project.update",
        json!({"project_id": project_id, "name": "renamed project"}),
    )
    .await
    .unwrap();

    host.handle_method("websocket.token", json!({}))
        .await
        .unwrap();
    let secret = profile_root.join("runtime/websocket/secret.yaml");
    assert!(secret.is_file());
    if locked {
        assert!(dwo_file_guard::is_protected(&secret));
    }
    host.handle_method("websocket.reset_token", json!({}))
        .await
        .unwrap();

    let (owner, _) = host.projects.locate_session(session_id.as_str()).unwrap();
    host.handle_method(
        "project.session.archive",
        json!({"project_id": owner.id, "session_id": session_id}),
    )
    .await
    .unwrap();
    host.delete_session(&session_id).await.unwrap();
    assert!(!session_file.exists());

    host.shutdown().await;
}

fn find_session_directory(root: &Path, session_id: &str) -> PathBuf {
    let mut directories = vec![root.join("runtime/sessions")];
    while let Some(directory) = directories.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            if entry.file_name().to_str() == Some(session_id) {
                return entry.path();
            }
            directories.push(entry.path());
        }
    }
    panic!("session directory for {session_id} was not found");
}
