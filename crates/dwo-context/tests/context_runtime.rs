use dwo_context::{
    ChannelCapabilitySnapshot, CompactionPlanner, ContentBlock, ContextManager, ContextMessage,
    MessageContent, MessageKind, MessageRole, SessionContext, SystemPromptBuilder,
    ToolResultRecord, estimate_content_tokens, estimate_context_tokens,
};
use serde_json::json;

fn write(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn responses_are_stored_item_first_and_provider_private_items_are_trimmed_on_switch() {
    let mut manager = ContextManager::new(SessionContext {
        messages: vec![
            ContextMessage::system("system"),
            ContextMessage::user("question"),
        ],
        ..SessionContext::default()
    });
    manager.append_response_items(
        "provider-a",
        vec![
            json!({"type":"reasoning", "summary":[{"type":"summary_text", "text":"think"}], "encrypted_content":"opaque"}),
            json!({"type":"web_search_call", "id":"search-1", "status":"completed"}),
            json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}),
            json!({"type":"function_call", "call_id":"call-1", "name":"terminal", "arguments":"{\"command\":\"pwd\"}"}),
        ],
    );
    manager.append_tool(ToolResultRecord {
        tool_call_id: "call-1".to_string(),
        tool_name: "terminal".to_string(),
        output: json!({"status":"completed", "output":"cwd"}),
        model_context: Vec::new(),
    });

    let response_items = manager
        .model_messages()
        .iter()
        .filter_map(ContextMessage::response_item_value)
        .collect::<Vec<_>>();
    assert_eq!(response_items.len(), 4);
    assert_eq!(
        manager
            .model_messages()
            .iter()
            .filter(|message| message.is_provider_owned())
            .count(),
        2
    );
    assert!(manager.normalize_for_selection("provider-a", None, true));
    assert!(!manager.normalize_for_selection("provider-a", None, true));
    assert!(manager.normalize_for_selection("provider-b", None, true));

    let kinds = manager
        .model_messages()
        .iter()
        .filter_map(ContextMessage::response_item_value)
        .filter_map(|item| item.get("type").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["message", "function_call"]);
    assert!(manager.model_messages().iter().any(|message| {
        message.role == MessageRole::Tool && message.tool_call_id.as_deref() == Some("call-1")
    }));
    assert_eq!(manager.context().provider.as_deref(), Some("provider-b"));
}

#[test]
fn legacy_aggregated_response_schema_is_rejected() {
    let error = serde_json::from_value::<ContextMessage>(json!({
        "role": "assistant",
        "content": [],
        "response_items": [{"type": "message", "role": "assistant", "content": []}]
    }))
    .unwrap_err();

    assert!(error.to_string().contains("unknown field `response_items`"));
}

#[test]
fn selection_normalization_permanently_removes_unsupported_images() {
    let mut manager = ContextManager::new(SessionContext {
        messages: vec![
            ContextMessage::system("system"),
            ContextMessage::user(MessageContent::blocks(vec![
                ContentBlock::text("inspect"),
                ContentBlock::image("image/png", "aGVsbG8="),
            ])),
        ],
        ..SessionContext::default()
    });

    assert!(manager.normalize_for_selection("provider-a", None, false));
    assert!(!manager.contains_images());
    assert_eq!(
        manager.model_messages()[1].content.as_text(),
        Some("inspect")
    );
    assert!(!manager.normalize_for_selection("provider-a", None, true));
    assert!(!manager.contains_images());
}

#[test]
fn usage_estimates_the_complete_context_without_provider_token_fields() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    let tools = vec![json!({"type":"function","function":{"name":"terminal"}})];

    let initial = manager.refresh_usage(&tools);
    manager.append_user("hello world");
    let with_user = manager.refresh_usage(&tools);
    assert_eq!(
        with_user,
        estimate_context_tokens(manager.model_messages(), &tools)
    );
    manager.record_model_success("second-model");

    assert!(initial > 0);
    assert!(with_user > initial);
    assert_eq!(manager.context().usage.current_tokens, with_user);
    assert_eq!(
        manager.context().usage.last_model.as_deref(),
        Some("second-model")
    );
    assert!(
        manager
            .scheduled_compaction(with_user + 1, &tools)
            .is_none()
    );
    assert!(manager.scheduled_compaction(with_user, &tools).is_some());

    let usage = serde_json::to_value(&manager.context().usage).unwrap();
    assert!(usage.get("input_tokens").is_none());
    assert!(usage.get("output_tokens").is_none());
    assert!(usage.get("total_tokens").is_none());
}

#[test]
fn prompt_uses_and_watches_only_profile_and_initial_cwd_agents_files() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let cwd = root.path().join("workspace").join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    write(
        &profile.join("resource/prompts/System.md"),
        "Profile system prompt",
    );
    write(
        &profile.join("resource/prompts/AGENTS.md"),
        "resource rule v1",
    );
    write(&cwd.join("AGENTS.md"), "cwd rule v1");
    write(&root.path().join("workspace/AGENTS.md"), "parent ignored");
    write(
        &profile.join("resource/skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: Demo skill\n---\nInstructions",
    );
    write(&profile.join("resource/mcp.json"), "{}");

    let builder = SystemPromptBuilder::new(Some(profile.clone()), cwd.clone());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    let prompt = manager.system_prompt();
    assert!(prompt.contains("Profile system prompt"));
    assert!(prompt.contains("resource rule v1"));
    assert!(prompt.contains("cwd rule v1"));
    assert!(!prompt.contains("parent ignored"));
    assert!(prompt.contains("Demo skill"));
    assert!(!prompt.contains("<mcp>"));
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 0);

    write(
        &profile.join("resource/prompts/AGENTS.md"),
        "resource rule v2",
    );
    std::fs::remove_file(cwd.join("AGENTS.md")).unwrap();
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let watcher = manager.model_messages().last().unwrap();
    assert_eq!(watcher.kind, MessageKind::EnvWatcher);
    assert!(watcher.content.contains("resource rule v2"));
    assert!(!watcher.content.contains("cwd rule v1"));
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 0);
}

#[test]
fn prompt_discovers_project_agents_rules_and_skills() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let cwd = root.path().join("workspace").join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    write(
        &profile.join("resource/prompts/System.md"),
        "Profile system prompt",
    );
    write(&profile.join("resource/prompts/AGENTS.md"), "resource rule");
    write(
        &profile.join("resource/skills/global-tool/SKILL.md"),
        "---\nname: global-tool\ndescription: Global tool\n---\nGlobal instructions",
    );
    write(
        &profile.join("resource/skills/shared/SKILL.md"),
        "---\nname: shared\ndescription: Profile shared skill\n---\nProfile version",
    );
    write(&cwd.join("AGENTS.md"), "cwd rule");
    write(&cwd.join(".agents/AGENTS.md"), "project rule v1");
    write(
        &cwd.join(".agents/skills/local-tool/SKILL.md"),
        "---\nname: local-tool\ndescription: Local tool\n---\nLocal instructions",
    );
    write(
        &cwd.join(".agents/skills/shared/SKILL.md"),
        "---\nname: shared\ndescription: Project shared skill\n---\nProject version",
    );

    let builder = SystemPromptBuilder::new(Some(profile.clone()), cwd.clone());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    let prompt = manager.system_prompt();
    assert!(prompt.contains("resource rule"));
    assert!(prompt.contains("cwd rule"));
    assert!(prompt.contains("project rule v1"));
    assert!(prompt.contains("Global tool"));
    assert!(prompt.contains("Local tool"));
    assert!(prompt.contains("Project shared skill"));
    assert!(!prompt.contains("Profile shared skill"));
    let expected_fragment = std::path::Path::new(".agents")
        .join("skills")
        .join("local-tool")
        .join("SKILL.md");
    assert!(prompt.contains(&expected_fragment.to_string_lossy().to_string()));

    std::fs::remove_file(cwd.join(".agents/AGENTS.md")).unwrap();
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let watcher = manager.model_messages().last().unwrap();
    assert_eq!(watcher.kind, MessageKind::EnvWatcher);
    assert!(!watcher.content.contains("project rule v1"));

    write(
        &cwd.join(".agents/skills/extra/SKILL.md"),
        "---\nname: extra\ndescription: Extra skill\n---\nExtra instructions",
    );
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let watcher = manager.model_messages().last().unwrap();
    assert_eq!(watcher.kind, MessageKind::EnvWatcher);
    assert!(watcher.content.contains("Extra skill"));
}

#[test]
fn prompt_discovers_external_skill_dirs_and_applies_project_priority() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let cwd = root.path().join("workspace").join("project");
    let external = root.path().join("shared-skills");
    std::fs::create_dir_all(&cwd).unwrap();
    write(
        &profile.join("resource/prompts/System.md"),
        "Profile system prompt",
    );
    write(
        &profile.join("resource/skills/builtin/SKILL.md"),
        "---\nname: builtin\ndescription: Builtin skill\n---\nBuiltin instructions",
    );
    write(
        &profile.join("resource/skills/shared/SKILL.md"),
        "---\nname: shared\ndescription: Profile shared skill\n---\nProfile version",
    );
    write(
        &external.join("external-tool/SKILL.md"),
        "---\nname: external-tool\ndescription: External tool\n---\nExternal instructions",
    );
    write(
        &external.join("shared/SKILL.md"),
        "---\nname: shared\ndescription: External shared skill\n---\nExternal version",
    );

    let dirs = std::sync::Arc::new(std::sync::RwLock::new(vec![external.clone()]));
    let builder = SystemPromptBuilder::new(Some(profile.clone()), cwd.clone())
        .with_external_skill_dirs(dirs.clone());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    let prompt = manager.system_prompt();
    assert!(prompt.contains("Builtin skill"));
    assert!(prompt.contains("External tool"));
    assert!(prompt.contains("External shared skill"));
    assert!(!prompt.contains("Profile shared skill"));

    // 项目级同名 skill 覆盖 external 目录
    write(
        &cwd.join(".agents/skills/shared/SKILL.md"),
        "---\nname: shared\ndescription: Project shared skill\n---\nProject version",
    );
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let watcher = manager.model_messages().last().unwrap();
    assert_eq!(watcher.kind, MessageKind::EnvWatcher);
    assert!(watcher.content.contains("Project shared skill"));
    assert!(!watcher.content.contains("External shared skill"));

    // 热更新：Arc 内容变化（模拟 service.replace_external_skill_dirs）
    *dirs.write().unwrap() = vec![];
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let watcher = manager.model_messages().last().unwrap();
    assert_eq!(watcher.kind, MessageKind::EnvWatcher);
    assert!(watcher.content.contains("Builtin skill"));
    assert!(watcher.content.contains("Project shared skill"));
    assert!(!watcher.content.contains("External tool"));
}

#[test]
fn prompt_adds_and_removes_adapter_projected_channel_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(profile.join("resource/prompts")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        profile.join("resource/prompts/System.md"),
        "You are an agent.",
    )
    .unwrap();
    let builder = SystemPromptBuilder::new(Some(profile.clone()), cwd)
        .with_channel_prompt("Channel command guidance");
    let mut manager = ContextManager::initialize(&builder).unwrap();
    assert!(manager.system_prompt().contains("<channels>"));
    assert!(manager.system_prompt().contains("Channel command guidance"));
    assert!(!manager.system_prompt().contains("name=\"weixin\""));

    ChannelCapabilitySnapshot::set_runtime(
        profile.clone(),
        vec![
            ChannelCapabilitySnapshot {
                name: "weixin".to_string(),
                content: "Use `dwo channel weixin send-message` and `send-file`.".to_string(),
            },
            ChannelCapabilitySnapshot {
                name: "telegram".to_string(),
                content: "Use `dwo channel telegram send-message` and `send-file`.".to_string(),
            },
        ],
    );
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let added = &manager.model_messages().last().unwrap().content;
    assert!(added.contains("<channels>"));
    assert!(added.contains("The available channel adapters changed:"));
    assert!(added.contains("<channel name=\"weixin\" state=\"available\">"));
    assert!(added.contains("dwo channel weixin send-message"));
    assert!(added.contains("<channel name=\"telegram\" state=\"available\">"));
    assert!(added.contains("dwo channel telegram send-message"));

    ChannelCapabilitySnapshot::set_runtime(
        profile.clone(),
        vec![ChannelCapabilitySnapshot {
            name: "telegram".to_string(),
            content: "Use `dwo channel telegram send-message` and `send-file`.".to_string(),
        }],
    );
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let removed = &manager.model_messages().last().unwrap().content;
    assert!(removed.contains("<channels>"));
    assert!(removed.contains("<channel name=\"weixin\" state=\"removed\">"));
    assert!(removed.contains("<channel name=\"telegram\" state=\"available\">"));
}

#[test]
fn prompt_renders_command_guidance_in_separate_xml_blocks() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path())
        .with_tool_prompt("tool guidance")
        .with_subsession_prompt("subsession guidance")
        .with_automation_prompt("automation guidance")
        .with_channel_prompt("channel guidance");
    let manager = ContextManager::initialize(&builder).unwrap();
    let prompt = manager.system_prompt();

    assert!(prompt.contains("<tools>\ntool guidance\n</tools>"));
    assert!(prompt.contains("<subsession>\nsubsession guidance\n</subsession>"));
    assert!(prompt.contains("<automation>\nautomation guidance\n</automation>"));
    assert!(prompt.contains("<channels>\nchannel guidance\n</channels>"));
}

#[test]
fn prompt_progressively_exposes_mcp_catalog_and_watches_configuration() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let cwd = root.path().join("workspace");
    write(
        &profile.join("resource/prompts/System.md"),
        "You are an agent.",
    );
    write(&profile.join("resource/mcp.json"), "{\"mcpServers\":{}}");
    std::fs::create_dir_all(&cwd).unwrap();
    let builder = SystemPromptBuilder::new(Some(profile.clone()), cwd);
    let mut manager = ContextManager::initialize(&builder).unwrap();
    assert!(!manager.system_prompt().contains("<mcp>"));

    write(
        &profile.join("resource/mcp.json"),
        r#"{"mcpServers":{"github":{"transport":"streamableHttp","url":"https://example.test/mcp"}}}"#,
    );
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let pending = &manager.model_messages().last().unwrap().content;
    assert!(pending.contains("github    ? tools    starting"));
    assert!(pending.contains("dwo mcp search <query>"));

    let fingerprint = builder
        .build_initial()
        .unwrap()
        .snapshot
        .unwrap()
        .mcp
        .unwrap()
        .fingerprint;
    write(
        &profile.join("runtime/mcp/catalog.json"),
        &serde_json::json!({
            "configFingerprint": fingerprint,
            "summary": "github    18 tools    ready"
        })
        .to_string(),
    );
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    assert!(
        manager
            .model_messages()
            .last()
            .unwrap()
            .content
            .contains("github    18 tools    ready")
    );

    write(&profile.join("resource/mcp.json"), "{\"mcpServers\":{}}");
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    assert!(
        manager
            .model_messages()
            .last()
            .unwrap()
            .content
            .contains("<mcp state=\"removed\">")
    );
}

#[test]
fn content_blocks_round_trip_through_context_storage() {
    let value = json!([
        {"type":"text", "text":"inspect"},
        {"type":"image", "mimeType":"image/png", "data":"iVBORw0KGgo=", "uri":"file:///shot.png"},
        {"type":"audio", "mimeType":"audio/wav", "data":"UklGRg=="},
        {"type":"resource", "resource":{"uri":"file:///main.rs", "mimeType":"text/rust", "text":"fn main() {}"}},
        {"type":"resource_link", "uri":"file:///guide.pdf", "name":"guide.pdf", "mimeType":"application/pdf", "size":42}
    ]);
    let content: MessageContent = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(serde_json::to_value(content).unwrap(), value);
}

#[test]
fn tool_batch_places_image_context_after_every_tool_result() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    manager.append_tool_batch(vec![
        ToolResultRecord {
            tool_call_id: "text-call".to_string(),
            tool_name: "read_file".to_string(),
            output: json!({"content":"hello", "start_line":1, "end_line":1}),
            model_context: Vec::new(),
        },
        ToolResultRecord {
            tool_call_id: "image-call".to_string(),
            tool_name: "read_file".to_string(),
            output: json!({"status":"completed"}),
            model_context: vec![MessageContent::blocks(vec![ContentBlock::image(
                "image/png",
                "aGVsbG8=",
            )])],
        },
    ]);

    let messages = manager.model_messages();
    let tail = &messages[messages.len() - 3..];
    assert_eq!(tail[0].role, MessageRole::Tool);
    assert_eq!(tail[1].role, MessageRole::Tool);
    assert_eq!(tail[2].role, MessageRole::User);
    assert!(tail[2].content.contains_images());
}

#[test]
fn compaction_sends_raw_history_to_summary_and_filters_only_the_reserve() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    manager.append_user("inspect the project");
    manager.append_assistant_with_reasoning(
        "checking",
        Some("keep this reasoning in summary input".to_string()),
        vec![json!({
            "id":"call-1",
            "name":"terminal",
            "arguments":{"action":"run","command":"rg TODO"}
        })],
    );
    manager.append_tool(ToolResultRecord {
        tool_call_id: "call-1".to_string(),
        tool_name: "terminal".to_string(),
        output: json!({"output":"src/main.rs:1:TODO"}),
        model_context: Vec::new(),
    });
    manager.append_user("continue");
    manager.append_assistant("done", Vec::new());

    let plan = manager.plan_compaction(&CompactionPlanner::new(20, 5_000));
    let historical_assistant = plan
        .view
        .messages
        .iter()
        .find(|message| !message.tool_calls.is_empty())
        .unwrap();
    let historical_result = plan
        .view
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .unwrap();

    assert_eq!(
        historical_assistant.reasoning.as_deref(),
        Some("keep this reasoning in summary input")
    );
    assert_eq!(
        historical_assistant.tool_calls[0]["arguments"]["command"],
        "rg TODO"
    );
    assert!(historical_result.content.contains("src/main.rs:1:TODO"));
    assert_eq!(plan.reserved_messages[0].content, "continue");
}

#[test]
fn historical_and_reserved_users_share_one_token_budget() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    for index in 1..=4 {
        manager.append_user(format!("HEAD-{index}-{}-TAIL-{index}", "you".repeat(40)));
        manager.append_assistant(format!("answer {index}"), Vec::new());
    }

    let plan = manager.plan_compaction(&CompactionPlanner::new(50, 40));
    let retained_user_tokens = plan
        .front_user_messages
        .iter()
        .chain(
            plan.reserved_messages
                .iter()
                .filter(|message| message.is_real_user()),
        )
        .map(|message| estimate_content_tokens(&message.content))
        .sum::<u64>();

    assert!(retained_user_tokens <= 40);
    assert!(plan.front_user_messages.len() <= 4);
}

#[test]
fn reserve_tool_filter_can_replace_context_without_a_summary_call() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    manager.append_user("inspect");
    manager.append_assistant(
        "",
        vec![json!({
            "id":"call-1",
            "name":"file_edit",
            "arguments":{"patch":"*** Begin Patch\n*** Add File: x\n+x\n*** End Patch"}
        })],
    );
    manager.append_tool(ToolResultRecord {
        tool_call_id: "call-1".to_string(),
        tool_name: "file_edit".to_string(),
        output: json!({"status":"completed", "changes":[{"path":"x", "kind":"add"}]}),
        model_context: Vec::new(),
    });

    let plan = manager.plan_compaction(&CompactionPlanner::new(10_000, 5_000));
    assert!(!plan.has_compactable_history());
    assert!(plan.needs_replacement());
    assert!(
        plan.reserved_messages[1].tool_calls[0]["arguments"]["patch"]
            .as_str()
            .unwrap()
            .contains("file patch omitted")
    );

    manager.apply_compaction(plan, "", &builder, &[]).unwrap();
    assert!(manager.context().usage.current_tokens > 0);
}

#[test]
fn compaction_projection_preserves_images_only_for_image_capable_models() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    for index in 1..=2 {
        manager.append_user(MessageContent::blocks(vec![
            ContentBlock::text(format!("user {index}")),
            ContentBlock::image("image/png", format!("image-{index}")),
        ]));
        manager.append_assistant(format!("answer {index}"), Vec::new());
    }

    let normal = manager.plan_compaction(&CompactionPlanner::new(1_500, 5_000));
    assert!(normal.view.messages.iter().any(|message| {
        message
            .content
            .as_blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    }));
    assert!(normal.reserved_messages.iter().any(|message| {
        message
            .content
            .as_blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    }));

    let image_projection = normal.clone().project_for_image_input(true);
    assert!(image_projection.view.messages.iter().any(|message| {
        message
            .content
            .as_blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    }));
    assert!(image_projection.reserved_messages.iter().any(|message| {
        message
            .content
            .as_blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    }));

    let text_projection = normal.project_for_image_input(false);
    let projected_images = text_projection
        .view
        .messages
        .iter()
        .chain(&text_projection.front_user_messages)
        .chain(&text_projection.reserved_messages)
        .flat_map(|message| message.content.as_blocks())
        .filter(|block| matches!(block, ContentBlock::Image { .. }))
        .count();
    assert_eq!(projected_images, 0);

    manager
        .apply_compaction(text_projection, "text-only summary", &builder, &[])
        .unwrap();
    assert!(!manager.contains_images());
    assert!(manager.context().usage.current_tokens > 0);
}

#[test]
fn compaction_rebuild_absorbs_current_rules_and_reestimates_usage() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let cwd = root.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    write(&profile.join("resource/prompts/System.md"), "system v1");
    write(&cwd.join("AGENTS.md"), "cwd rules v1");
    let builder = SystemPromptBuilder::new(Some(profile.clone()), cwd.clone());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    manager.append_user("hello");

    write(&profile.join("resource/prompts/System.md"), "system v2");
    write(&cwd.join("AGENTS.md"), "cwd rules v2");
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 2);
    let plan = manager.plan_compaction(&CompactionPlanner::new(1, 5_000));
    manager
        .apply_compaction(plan, "summary", &builder, &[])
        .unwrap();

    assert!(manager.system_prompt().contains("system v2"));
    assert!(manager.system_prompt().contains("cwd rules v2"));
    assert!(!manager.system_prompt().contains("cwd rules v1"));
    assert!(manager.context().usage.current_tokens > 0);
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 0);
}
