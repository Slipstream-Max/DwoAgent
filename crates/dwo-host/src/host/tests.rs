use super::*;
use dwo_agent_service::SessionListQuery;

pub(crate) fn write_test_profile(root: &Path) -> PathBuf {
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

    host.handle_method("session.archive", json!({"session_id": session_id}))
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

#[tokio::test]
async fn missing_external_workspace_is_tolerated_in_list_and_repaired_on_load() {
    let root = tempfile::tempdir().unwrap();
    let host = Host::build(&write_test_profile(root.path())).await.unwrap();
    let cwd = root.path().join("vanishing-workdir");
    std::fs::create_dir_all(&cwd).unwrap();
    let id = host
        .create_session(HostSessionOptions {
            cwd: Some(cwd.clone()),
            ..HostSessionOptions::default()
        })
        .await
        .unwrap();
    host.service.unload(&id).await.unwrap();
    std::fs::remove_dir_all(&cwd).unwrap();

    // A stale workspace path must not break listings.
    let page = host
        .service
        .list(SessionListQuery::new(None, None))
        .await
        .unwrap();
    assert!(page.sessions.iter().any(|item| item.session_id == id));

    // Loading the session repairs it onto a freshly created managed workspace.
    host.service.load(&id).await.unwrap();
    let snapshot = host.service.snapshot(&id).await.unwrap();
    assert!(matches!(
        snapshot.record.info.workspace,
        SessionWorkspace::Managed
    ));
    assert!(snapshot.record.info.cwd.is_dir());
    host.shutdown().await;
}

#[tokio::test]
async fn project_folder_loss_relocates_the_project_and_rebinds_sessions() {
    let root = tempfile::tempdir().unwrap();
    let host = Host::build(&write_test_profile(root.path())).await.unwrap();
    let project_pwd = root.path().join("project-root");
    std::fs::create_dir_all(&project_pwd).unwrap();
    let project = host
        .handle_method(
            "project.create",
            json!({"name": "Relocated", "pwd": &project_pwd}),
        )
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap().to_string();
    let first = host
        .handle_method(
            "session.new",
            json!({
                "title": "first",
                "cwd": null,
                "project_id": project_id,
                "section_id": null,
                "worktree_id": null,
                "caller_session_id": null,
            }),
        )
        .await
        .unwrap();
    let first_id = SessionId::parse(first["session_id"].as_str().unwrap().to_string())
        .map_err(anyhow::Error::msg)
        .unwrap();
    let second = host
        .handle_method(
            "session.new",
            json!({
                "title": "second",
                "cwd": null,
                "project_id": project_id,
                "section_id": null,
                "worktree_id": null,
                "caller_session_id": null,
            }),
        )
        .await
        .unwrap();
    let second_id = SessionId::parse(second["session_id"].as_str().unwrap().to_string())
        .map_err(anyhow::Error::msg)
        .unwrap();
    let expected_project_dir = std::fs::canonicalize(&project_pwd).unwrap();
    assert_eq!(
        host.service
            .snapshot(&first_id)
            .await
            .unwrap()
            .record
            .info
            .cwd,
        expected_project_dir
    );

    // The project folder disappears; opening one session relocates the project.
    host.service.unload(&first_id).await.unwrap();
    host.service.unload(&second_id).await.unwrap();
    std::fs::remove_dir_all(&project_pwd).unwrap();
    host.service.load(&first_id).await.unwrap();

    let workspace =
        std::fs::canonicalize(root.path().join("runtime/workspaces").join(&project_id)).unwrap();
    assert!(workspace.is_dir());
    assert_eq!(
        host.service
            .snapshot(&first_id)
            .await
            .unwrap()
            .record
            .info
            .cwd,
        workspace
    );
    assert_eq!(
        host.service
            .snapshot(&second_id)
            .await
            .unwrap()
            .record
            .info
            .cwd,
        workspace
    );
    let project = host
        .handle_method("project.get", json!({"project_id": project_id}))
        .await
        .unwrap();
    assert_eq!(PathBuf::from(project["pwd"].as_str().unwrap()), workspace);
    host.shutdown().await;
}

#[tokio::test]
async fn project_pwd_change_rebinds_all_sessions() {
    let root = tempfile::tempdir().unwrap();
    let host = Host::build(&write_test_profile(root.path())).await.unwrap();
    let first_dir = root.path().join("project-one");
    let second_dir = root.path().join("project-two");
    std::fs::create_dir_all(&first_dir).unwrap();
    std::fs::create_dir_all(&second_dir).unwrap();
    let project = host
        .handle_method(
            "project.create",
            json!({"name": "Moving", "pwd": &first_dir}),
        )
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap().to_string();
    let session = host
        .handle_method(
            "session.new",
            json!({
                "title": "mover",
                "cwd": null,
                "project_id": project_id,
                "section_id": null,
                "worktree_id": null,
                "caller_session_id": null,
            }),
        )
        .await
        .unwrap();
    let session_id = SessionId::parse(session["session_id"].as_str().unwrap().to_string())
        .map_err(anyhow::Error::msg)
        .unwrap();
    let before = std::fs::canonicalize(&first_dir).unwrap();
    assert_eq!(
        host.service
            .snapshot(&session_id)
            .await
            .unwrap()
            .record
            .info
            .cwd,
        before
    );

    host.handle_method(
        "project.update",
        json!({"project_id": project_id, "name": null, "pwd": &second_dir}),
    )
    .await
    .unwrap();

    let after = std::fs::canonicalize(&second_dir).unwrap();
    assert_eq!(
        host.service
            .snapshot(&session_id)
            .await
            .unwrap()
            .record
            .info
            .cwd,
        after
    );
    let project = host
        .handle_method("project.get", json!({"project_id": project_id}))
        .await
        .unwrap();
    assert_eq!(PathBuf::from(project["pwd"].as_str().unwrap()), after);
    host.shutdown().await;
}

#[tokio::test]
async fn failed_project_repair_keeps_the_link_and_retries_on_the_next_load() {
    let root = tempfile::tempdir().unwrap();
    let host = Host::build(&write_test_profile(root.path())).await.unwrap();
    let project_pwd = root.path().join("retrying-project");
    std::fs::create_dir_all(&project_pwd).unwrap();
    let project = host
        .handle_method(
            "project.create",
            json!({"name": "Retrying", "pwd": &project_pwd}),
        )
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap().to_string();
    let session = host
        .handle_method(
            "session.new",
            json!({
                "title": "retrier",
                "cwd": null,
                "project_id": project_id,
                "section_id": null,
                "worktree_id": null,
                "caller_session_id": null,
            }),
        )
        .await
        .unwrap();
    let session_id = SessionId::parse(session["session_id"].as_str().unwrap().to_string())
        .map_err(anyhow::Error::msg)
        .unwrap();
    let expected_project_dir = std::fs::canonicalize(&project_pwd).unwrap();

    // Block the would-be project workspace with a regular file so the repair fails.
    host.service.unload(&session_id).await.unwrap();
    std::fs::remove_dir_all(&project_pwd).unwrap();
    let blocked = root.path().join("runtime/workspaces").join(&project_id);
    std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    std::fs::write(&blocked, "blocked").unwrap();

    // The load still succeeds, served from a managed directory, and the record keeps
    // pointing at the project so the next load retries.
    host.service.load(&session_id).await.unwrap();
    let snapshot = host.service.snapshot(&session_id).await.unwrap();
    assert!(matches!(
        snapshot.record.info.workspace,
        SessionWorkspace::External { .. }
    ));
    let managed = std::fs::canonicalize(
        root.path()
            .join("runtime/workspaces")
            .join(session_id.as_str()),
    )
    .unwrap();
    assert_eq!(snapshot.record.info.cwd, managed);
    let project = host
        .handle_method("project.get", json!({"project_id": project_id}))
        .await
        .unwrap();
    assert_eq!(
        PathBuf::from(project["pwd"].as_str().unwrap()),
        expected_project_dir
    );

    // Unblock and reload: the retry goes through and re-points the project and session.
    std::fs::remove_file(&blocked).unwrap();
    host.service.unload(&session_id).await.unwrap();
    host.service.load(&session_id).await.unwrap();
    let workspace =
        std::fs::canonicalize(root.path().join("runtime/workspaces").join(&project_id)).unwrap();
    let snapshot = host.service.snapshot(&session_id).await.unwrap();
    assert_eq!(snapshot.record.info.cwd, workspace);
    let project = host
        .handle_method("project.get", json!({"project_id": project_id}))
        .await
        .unwrap();
    assert_eq!(PathBuf::from(project["pwd"].as_str().unwrap()), workspace);
    host.shutdown().await;
}
