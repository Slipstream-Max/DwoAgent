use super::channel_api::{ManagedChannelAction, managed_channel_action};
use super::session_api::{PromptMessage, PromptParam, ensure_policy_ceiling};
use super::*;
use dwo_agent_service::{SessionConfigUpdate, SessionListQuery};
use dwo_channels::ChannelKind;
use dwo_context::MessageContent;

#[test]
fn prompt_message_accepts_text_and_structured_content() {
    let text: PromptMessage = serde_json::from_value(json!("hello")).unwrap();
    assert_eq!(text.into_content(), MessageContent::text("hello"));

    let content: PromptMessage = serde_json::from_value(json!([
        {"type": "text", "text": "inspect"},
        {"type": "image", "mimeType": "image/png", "data": "aGVsbG8="}
    ]))
    .unwrap();
    let content = content.into_content();
    assert_eq!(content.as_blocks().len(), 2);
    assert!(content.contains_images());
}

#[test]
fn managed_channel_actions_share_one_rpc_route() {
    for channel in ChannelKind::ALL {
        let method = |action| format!("channel.{}.{action}", channel.as_str());
        assert!(matches!(
            managed_channel_action(&method("status")),
            Some((found, ManagedChannelAction::Status)) if found == channel
        ));
        assert!(matches!(
            managed_channel_action(&method("send_message")),
            Some((found, ManagedChannelAction::SendMessage)) if found == channel
        ));
        assert!(matches!(
            managed_channel_action(&method("send_file")),
            Some((found, ManagedChannelAction::SendFile)) if found == channel
        ));
        assert!(matches!(
            managed_channel_action(&method("remove")),
            Some((found, ManagedChannelAction::Remove)) if found == channel
        ));
        assert!(managed_channel_action(&method("begin")).is_none());
    }
}

#[test]
fn subsession_policy_cannot_exceed_parent() {
    assert!(ensure_policy_ceiling(SessionMode::Watch, SessionMode::Confirm).is_ok());
    assert!(ensure_policy_ceiling(SessionMode::Confirm, SessionMode::Confirm).is_ok());
    assert!(ensure_policy_ceiling(SessionMode::FullAccess, SessionMode::Confirm).is_err());
    assert!(ensure_policy_ceiling(SessionMode::Confirm, SessionMode::Watch).is_err());
}

fn write_test_profile(root: &Path) -> PathBuf {
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
async fn management_capabilities_report_only_live_contracts() {
    let root = tempfile::tempdir().unwrap();
    let host = Host::build(&write_test_profile(root.path())).await.unwrap();
    let capabilities = host
        .handle_method("dwo.capabilities", json!({}))
        .await
        .unwrap();
    assert_eq!(capabilities["protocolVersion"], 3);
    assert_eq!(capabilities["route"], "dwo");
    assert_eq!(capabilities["eventCursor"], true);
    let methods = capabilities["methods"].as_array().unwrap();
    assert!(methods.iter().any(|method| method == "session.list"));
    assert!(!methods.iter().any(|method| method == "session.status-list"));
    assert!(methods.iter().any(|method| method == "mcp.auth.login"));
    assert!(methods.iter().any(|method| method == "skill.install"));
    host.shutdown().await;
}

#[tokio::test]
async fn project_board_composes_topics_sessions_labels_and_rules() {
    let root = tempfile::tempdir().unwrap();
    let config = write_test_profile(root.path());
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let host = Host::build(&config).await.unwrap();

    let project = host
        .handle_method("project.create", json!({"name": "Demo", "pwd": workspace}))
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap();
    let section_id = project["board"]["uncategorizedSectionId"].as_str().unwrap();
    let topic = host
        .handle_method(
            "project.topic.create",
            json!({
                "project_id": project_id,
                "section_id": section_id,
                "title": "Project API"
            }),
        )
        .await
        .unwrap();
    let topic_id = topic["id"].as_str().unwrap();
    host.handle_method(
        "project.topic.agents.set",
        json!({
            "project_id": project_id,
            "topic_id": topic_id,
            "content": "Keep changes inside the project API."
        }),
    )
    .await
    .unwrap();
    let label = host
        .handle_method(
            "project.label.create",
            json!({
                "project_id": project_id,
                "name": "Backend",
                "color": "#388E3C"
            }),
        )
        .await
        .unwrap();
    host.handle_method(
        "project.label.assign",
        json!({
            "project_id": project_id,
            "topic_id": topic_id,
            "label_id": label["id"]
        }),
    )
    .await
    .unwrap();
    let created = host
        .handle_method(
            "session.new",
            json!({"project_id": project_id, "topic_id": topic_id}),
        )
        .await
        .unwrap();
    let session_id = created["session_id"].as_str().unwrap();

    let detail = host
        .handle_method(
            "project.topic.get",
            json!({"project_id": project_id, "topic_id": topic_id}),
        )
        .await
        .unwrap();
    assert_eq!(detail["sessions"][0]["record"]["info"]["id"], session_id);
    assert_eq!(detail["labels"][0]["name"], "Backend");
    let snapshot = host
        .service
        .snapshot(&SessionId::parse(session_id.to_string()).unwrap())
        .await
        .unwrap();
    assert!(
        snapshot
            .record
            .context
            .system_prompt
            .content
            .contains("Keep changes inside the project API.")
    );
    assert!(
        snapshot.record.context.system_prompt.content.contains(
            &std::fs::canonicalize(workspace)
                .unwrap()
                .to_string_lossy()
                .to_string()
        )
    );
    let second = host
        .handle_method(
            "project.topic.create",
            json!({
                "project_id": project_id,
                "section_id": section_id,
                "title": "Review"
            }),
        )
        .await
        .unwrap();
    let second_id = second["id"].as_str().unwrap();
    host.handle_method(
        "project.topic.agents.set",
        json!({
            "project_id": project_id,
            "topic_id": second_id,
            "content": "Review the completed implementation."
        }),
    )
    .await
    .unwrap();
    host.handle_method(
        "project.topic.session.assign",
        json!({
            "project_id": project_id,
            "topic_id": second_id,
            "session_id": session_id
        }),
    )
    .await
    .unwrap();
    assert!(
        host.projects
            .get(project_id)
            .unwrap()
            .board
            .topics
            .iter()
            .find(|topic| topic.id == topic_id)
            .unwrap()
            .session_ids
            .is_empty()
    );
    assert!(
        host.projects
            .get(project_id)
            .unwrap()
            .board
            .topics
            .iter()
            .find(|topic| topic.id == second_id)
            .unwrap()
            .session_ids
            .iter()
            .any(|id| id == session_id)
    );
    host.handle_method(
        "project.topic.task.create",
        json!({
            "project_id": project_id,
            "topic_id": second_id,
            "job": {
                "name": "topic-review",
                "enabled": true,
                "schedule": {"cron": "0 9 * * *", "timezone": "Asia/Shanghai"},
                "session": {"mode": "new", "behavior": "every_time", "cwd": "."},
                "prompt": "Review now"
            }
        }),
    )
    .await
    .unwrap();
    let run = host
        .handle_method(
            "automation.run",
            json!({"job": "topic-review", "caller_session_id": null}),
        )
        .await
        .unwrap();
    let automation_session_id = run["sessionId"].as_str().unwrap();
    let (_, automation_topic) = host
        .projects
        .locate_session(automation_session_id)
        .expect("topic automation session is assigned to its topic");
    assert_eq!(automation_topic.id, second_id);
    let automation_snapshot = host
        .service
        .snapshot(&SessionId::parse(automation_session_id.to_string()).unwrap())
        .await
        .unwrap();
    assert!(
        automation_snapshot
            .record
            .context
            .system_prompt
            .content
            .contains("Review the completed implementation.")
    );
    host.shutdown().await;
}

#[tokio::test]
async fn prompt_directives_use_the_effective_session_skill_catalog() {
    let root = tempfile::tempdir().unwrap();
    let config = write_test_profile(root.path());
    let profile_skill = root.path().join("resource/skills/shared");
    std::fs::create_dir_all(&profile_skill).unwrap();
    std::fs::write(
        profile_skill.join("SKILL.md"),
        "---\nname: shared\ndescription: profile version\n---\nProfile instructions",
    )
    .unwrap();
    let project = root.path().join("project");
    let project_skill = project.join(".agents/skills/shared");
    std::fs::create_dir_all(&project_skill).unwrap();
    std::fs::write(
        project_skill.join("SKILL.md"),
        "---\nname: shared\ndescription: project version\n---\nProject instructions",
    )
    .unwrap();

    let host = Host::build(&config).await.unwrap();
    let expanded = host
        .expand_prompt_directives(
            &project,
            MessageContent::text(
                "use /skill shared now; keep /skill missing and bare /mcp unchanged",
            ),
        )
        .await
        .unwrap();
    let text = expanded.as_text().unwrap();
    let expected_path = std::fs::canonicalize(project_skill.join("SKILL.md")).unwrap();
    assert!(text.contains(&expected_path.display().to_string()));
    assert!(!text.contains(&profile_skill.display().to_string()));
    assert!(text.contains("/skill missing"));
    assert!(text.contains("bare /mcp unchanged"));

    let session_id = host
        .create_session(HostSessionOptions {
            cwd: Some(project),
            ..HostSessionOptions::default()
        })
        .await
        .unwrap();
    let options = host.prompt_directive_options(&session_id).await.unwrap();
    assert_eq!(options["skills"][0]["name"], "shared");
    assert_eq!(options["skills"][0]["description"], "project version");
    host.shutdown().await;
}

#[tokio::test]
async fn prompt_from_forks_a_direct_child_and_rejects_to() {
    let root = tempfile::tempdir().unwrap();
    let host = Host::build(&write_test_profile(root.path())).await.unwrap();
    let parent_id = host
        .create_session(HostSessionOptions {
            title: Some("parent".to_string()),
            ..HostSessionOptions::default()
        })
        .await
        .unwrap();
    let create = PromptParam {
        session_id: None,
        from_session_id: None,
        caller_session_id: None,
        endpoint_id: "test".to_string(),
        message: PromptMessage::Text("unused".to_string()),
        title: Some("child".to_string()),
        cwd: None,
        policy: None,
        model: None,
        reasoning: None,
        ephemeral: false,
    };
    let (source_id, _) = host
        .resolve_prompt_session(&create, Some(parent_id.clone()))
        .await
        .unwrap();
    let source_snapshot = host.service.snapshot(&source_id).await.unwrap();
    let fork = PromptParam {
        from_session_id: Some(source_id.to_string()),
        title: Some("forked child".to_string()),
        ..create
    };

    let (forked_id, returned_parent) = host
        .resolve_prompt_session(&fork, Some(parent_id.clone()))
        .await
        .unwrap();
    let forked_snapshot = host.service.snapshot(&forked_id).await.unwrap();

    assert_ne!(forked_id, source_id);
    assert_eq!(returned_parent.as_ref(), Some(&parent_id));
    assert_eq!(
        forked_snapshot.record.info.parent_session_id.as_ref(),
        Some(&parent_id)
    );
    assert_eq!(forked_snapshot.record.info.title, "forked child");
    assert_eq!(
        forked_snapshot.record.context,
        source_snapshot.record.context
    );

    let slash_fork = host
        .handle_method("session.fork", json!({"session_id": source_id.to_string()}))
        .await
        .unwrap();
    assert_eq!(slash_fork["accepted"], false);
    assert_ne!(slash_fork["session_id"], source_id.as_str());

    let invalid = PromptParam {
        session_id: Some(source_id.to_string()),
        from_session_id: Some(source_id.to_string()),
        ..fork
    };
    let error = host
        .resolve_prompt_session(&invalid, Some(parent_id.clone()))
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "--from cannot be used with --to");
    host.shutdown().await;
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
automation:
  enabled: false
  jobs: []
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
async fn automation_crud_updates_profile_and_runtime_together() {
    let root = tempfile::tempdir().unwrap();
    let config = write_test_profile(root.path());
    let host = Host::build(&config).await.unwrap();
    let job = json!({
        "name": "daily-report",
        "enabled": true,
        "schedule": {"cron": "0 9 * * *", "timezone": "Asia/Shanghai"},
        "session": {"mode": "new", "behavior": "every_time", "cwd": "."},
        "prompt": "summarize the project"
    });

    let added = host
        .handle_method("automation.add", json!({"job": job}))
        .await
        .unwrap();
    assert_eq!(added["job"]["name"], "daily-report");
    assert_eq!(host.automation.list().await.len(), 1);

    host.handle_method(
        "automation.disable",
        json!({"job": "daily-report", "all": false}),
    )
    .await
    .unwrap();
    assert!(
        !host
            .automation
            .status("daily-report")
            .await
            .unwrap()
            .job
            .enabled
    );

    host.handle_method("automation.enable", json!({"job": null, "all": true}))
        .await
        .unwrap();
    assert!(
        host.automation
            .status("daily-report")
            .await
            .unwrap()
            .job
            .enabled
    );

    host.handle_method(
        "automation.delete",
        json!({"job": "daily-report", "all": false}),
    )
    .await
    .unwrap();
    assert!(host.automation.list().await.is_empty());
    let profile = dwo_agent_service::AgentProfileConfig::load(root.path()).unwrap();
    assert!(
        parse_automation_config(profile.automation)
            .unwrap()
            .jobs
            .is_empty()
    );

    host.shutdown().await;
}

#[tokio::test]
async fn management_domains_mutate_through_host_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let config = write_test_profile(root.path());
    let host = Host::build(&config).await.unwrap();

    host.handle_method(
            "skill.install",
            json!({"name": "review", "content": "---\nname: review\ndescription: Review changes\n---\nRead the diff."}),
        )
        .await
        .unwrap();
    let listed = host.handle_method("skill.list", json!({})).await.unwrap();
    assert_eq!(listed["skills"][0]["name"], "review");
    host.handle_method("skill.disable", json!({"name": "review"}))
        .await
        .unwrap();
    let listed = host.handle_method("skill.list", json!({})).await.unwrap();
    assert_eq!(listed["disabled"][0], "review");
    host.handle_method("skill.enable", json!({"name": "review"}))
        .await
        .unwrap();
    host.handle_method("skill.uninstall", json!({"name": "review"}))
        .await
        .unwrap();

    host.handle_method(
        "prompt.set",
        json!({"name": "System.md", "content": "Updated system prompt."}),
    )
    .await
    .unwrap();
    let prompt = host
        .handle_method("prompt.get", json!({"name": "System.md"}))
        .await
        .unwrap();
    assert_eq!(prompt["content"], "Updated system prompt.");

    host.handle_method(
        "provider.upsert",
        json!({
            "name": "private",
            "provider": {
                "baseUrl": "https://private.example.com/v1",
                "apiKey": "secret",
                "models": {
                    "Private GPT": {
                        "modelId": "private-gpt",
                        "profile": "openai/gpt-5.6-terra"
                    }
                }
            }
        }),
    )
    .await
    .unwrap();
    let providers = host
        .handle_method("provider.list", json!({}))
        .await
        .unwrap();
    assert_eq!(providers["private"]["apiKeyConfigured"], true);
    assert!(providers["private"].get("apiKey").is_none());

    host.handle_method(
        "model.upsert",
        json!({
            "provider": "private",
            "name": "Private Grok",
            "model": {
                "modelId": "private-grok",
                "profile": "grok/grok-4.6"
            }
        }),
    )
    .await
    .unwrap();
    host.handle_method(
        "model.set_default",
        json!({"model": "private/private-grok", "reasoning": "High"}),
    )
    .await
    .unwrap();
    let session_id = host
        .create_session(HostSessionOptions {
            title: Some("model default".to_string()),
            ..HostSessionOptions::default()
        })
        .await
        .unwrap();
    let record = host.service.snapshot(&session_id).await.unwrap().record;
    assert_eq!(record.llm.model, "private/private-grok");
    assert_eq!(record.llm.reasoning.as_deref(), Some("High"));

    host.handle_method(
        "model.catalog.upsert",
        json!({
            "family": "minimax",
            "spec": {
                "models": {
                    "minimax-m2.5": {
                        "contextWindowTokens": 200000,
                        "maxOutputTokens": 32000
                    }
                }
            }
        }),
    )
    .await
    .unwrap();
    let catalog = host
        .handle_method("model.catalog.list", json!({}))
        .await
        .unwrap();
    assert_eq!(
        catalog["families"]["minimax"]["models"]["minimax-m2.5"]["maxOutputTokens"],
        32000
    );
    host.handle_method("model.catalog.remove", json!({"family": "minimax"}))
        .await
        .unwrap();

    host.handle_method(
        "mcp.install",
        json!({"server": "disabled-tool", "config": {"command": "missing-dwo-mcp"}}),
    )
    .await
    .unwrap();
    host.handle_method("mcp.disable", json!({"server": "disabled-tool"}))
        .await
        .unwrap();
    let mcp = host.handle_method("mcp.config", json!({})).await.unwrap();
    assert_eq!(mcp["servers"][0]["enabled"], false);

    host.handle_method("config.update", json!({"maxModelSteps": 17}))
        .await
        .unwrap();
    let config = host
        .handle_method("config.snapshot", json!({}))
        .await
        .unwrap();
    assert_eq!(config["maxModelSteps"], 17);
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
async fn sessions_use_project_workspaces_and_external_rule_files() {
    let profile = tempfile::tempdir().unwrap();
    let config = write_test_profile(profile.path());
    let host = Host::build(&config).await.unwrap();

    let generated_id = host
        .create_session(HostSessionOptions::default())
        .await
        .unwrap();
    let generated_snapshot = host.service.snapshot(&generated_id).await.unwrap();
    let generated_cwd = generated_snapshot.record.info.cwd.clone();
    let (generated_project, generated_topic) = host
        .projects
        .locate_session(generated_id.as_str())
        .expect("generated session belongs to the uncategorized topic");
    assert_eq!(generated_cwd, generated_project.pwd);
    assert_eq!(
        generated_topic.id,
        generated_project.board.uncategorized_topic_id
    );
    assert!(generated_cwd.is_dir());
    let explicit = profile.path().join("projects/demo");
    std::fs::create_dir_all(&explicit).unwrap();
    let custom_id = host
        .create_session(HostSessionOptions {
            cwd: Some(PathBuf::from("projects/demo")),
            ..HostSessionOptions::default()
        })
        .await
        .unwrap();
    let custom_cwd = host
        .service
        .snapshot(&custom_id)
        .await
        .unwrap()
        .record
        .info
        .cwd;
    assert_eq!(custom_cwd, std::fs::canonicalize(&explicit).unwrap());
    let second_custom_id = host
        .create_session(HostSessionOptions {
            title: Some("second".to_string()),
            cwd: Some(PathBuf::from("projects/demo")),
            ..HostSessionOptions::default()
        })
        .await
        .unwrap();
    let (custom_project, custom_topic) = host.projects.locate_session(custom_id.as_str()).unwrap();
    let (second_project, second_topic) = host
        .projects
        .locate_session(second_custom_id.as_str())
        .unwrap();
    assert_eq!(custom_project.id, second_project.id);
    assert_eq!(custom_topic.id, second_topic.id);
    assert_eq!(second_topic.session_ids.len(), 2);
    assert_eq!(host.projects.list().len(), 2);

    for date in ["2026/07/15", "2026/07/16"] {
        let attachment = profile
            .path()
            .join("runtime/attachments/weixin")
            .join(date)
            .join(generated_id.as_str())
            .join("image.jpg");
        std::fs::create_dir_all(attachment.parent().unwrap()).unwrap();
        std::fs::write(attachment, b"image").unwrap();
    }

    host.delete_session(&generated_id).await.unwrap();
    assert!(
        generated_cwd.exists(),
        "the workspace belongs to Project, not Session"
    );
    assert!(
        host.projects
            .locate_session(generated_id.as_str())
            .is_none()
    );
    assert!(
        !profile
            .path()
            .join("runtime/attachments/weixin/2026/07/15")
            .join(generated_id.as_str())
            .exists()
    );
    host.delete_session(&custom_id).await.unwrap();
    host.delete_session(&second_custom_id).await.unwrap();
    assert!(explicit.is_dir(), "an explicit cwd must never be deleted");

    host.shutdown.cancel();
    host.service.shutdown().await;
}

#[tokio::test]
async fn automation_run_returns_after_the_run_is_queued() {
    let profile = tempfile::tempdir().unwrap();
    let config = write_test_profile(profile.path());
    let mut source = std::fs::read_to_string(&config).unwrap();
    source.push_str(
        r#"
automation:
  enabled: false
  jobs:
    - name: background-failure
      schedule: { cron: "0 9 * * *", timezone: Asia/Shanghai }
      session: { mode: new, behavior: every_time, cwd: definitely-missing }
      prompt: this must be submitted in the background
    - name: valid-start
      schedule: { cron: "0 9 * * *", timezone: Asia/Shanghai }
      session: { mode: new, behavior: every_time, cwd: . }
      prompt: this starts before the command returns
"#,
    );
    std::fs::write(&config, source).unwrap();
    let host = Host::build(&config).await.unwrap();

    let error = host
        .handle_method(
            "automation.run",
            json!({"job": "background-failure", "caller_session_id": null}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cannot find the file"));

    let value = host
        .handle_method(
            "automation.run",
            json!({"job": "valid-start", "caller_session_id": null}),
        )
        .await
        .unwrap();
    let record: crate::automation::AutomationRunRecord = serde_json::from_value(value).unwrap();
    assert_eq!(
        record.status,
        crate::automation::AutomationRunStatus::Queued
    );
    assert!(record.run_id.starts_with("run-"));
    assert!(record.session_id.is_some());
    assert!(record.turn_id.is_none());

    host.shutdown.cancel();
    host.service.shutdown().await;
}
