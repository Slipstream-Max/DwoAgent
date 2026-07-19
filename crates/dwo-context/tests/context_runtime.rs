use dwo_context::{
    CompactionPlanner, ContentBlock, ContextManager, ContextMessage, MessageContent, MessageKind,
    MessageRole, SystemPromptBuilder, ToolResultRecord, TurnId,
};
use serde_json::json;

fn write(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn usage_tracks_the_current_context_without_cumulative_token_fields() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();

    manager.record_turn_usage("first-model", 120);
    manager.record_turn_usage("second-model", 45);

    assert_eq!(manager.context().usage.current_tokens, 45);
    assert_eq!(
        manager.context().usage.last_model.as_deref(),
        Some("second-model")
    );
    assert!(!manager.should_compact(46));
    assert!(manager.should_compact(45));

    let usage = serde_json::to_value(&manager.context().usage).unwrap();
    assert_eq!(usage["current_tokens"], 45);
    assert!(usage.get("input_tokens").is_none());
    assert!(usage.get("output_tokens").is_none());
    assert!(usage.get("last_turn_input_tokens").is_none());
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
fn prompt_adds_and_removes_bound_weixin_capability_without_exposing_secrets() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(profile.join("resource/prompts")).unwrap();
    std::fs::create_dir_all(profile.join("channels/weixin")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        profile.join("resource/prompts/System.md"),
        "You are an agent.",
    )
    .unwrap();
    write(
        &profile.join("profile.yaml"),
        "channels:\n  weixin:\n    enabled: true\n",
    );

    let builder = SystemPromptBuilder::new(Some(profile.clone()), cwd);
    let mut manager = ContextManager::initialize(&builder).unwrap();
    assert!(
        !manager
            .system_prompt()
            .contains("<channel name=\"weixin\">")
    );

    write(
        &profile.join("channels/weixin/secret.yaml"),
        "botToken: super-secret-token\nbaseUrl: https://example.test\nilinkBotId: bot\nboundUserId: user\n",
    );
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let added = &manager.model_messages().last().unwrap().content;
    assert!(added.contains("<channel name=\"weixin\">"));
    assert!(added.contains("dwo channel weixin send-message"));
    assert!(!added.contains("super-secret-token"));

    std::fs::remove_file(profile.join("channels/weixin/secret.yaml")).unwrap();
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 1);
    let removed = &manager.model_messages().last().unwrap().content;
    assert!(removed.contains("state=\"removed\""));
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
fn compaction_keeps_tool_pairs_and_filters_reasoning_from_summary_history() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();

    for index in 1..=3 {
        manager.append_user(
            TurnId::parse(format!("turn-{index}")).unwrap(),
            format!("user {index}"),
        );
    }
    manager.append_assistant_with_reasoning(
        TurnId::parse("turn-3").unwrap(),
        "reasoned answer",
        Some("embedded private reasoning".to_string()),
        Vec::new(),
    );
    let long_command = "echo terminal-command ".repeat(300);
    let calls = vec![
        json!({"id":"paired", "name":"terminal", "arguments":{"action":"run", "command":long_command}}),
        json!({"id":"missing", "name":"terminal", "arguments":{"action":"run"}}),
    ];
    manager.append_assistant(TurnId::parse("turn-3").unwrap(), "running", calls);
    manager.append_tool(
        TurnId::parse("turn-3").unwrap(),
        ToolResultRecord {
            tool_call_id: "paired".to_string(),
            tool_name: "terminal".to_string(),
            output: json!({"status":"completed", "output": "x".repeat(500)}),
        },
    );
    let mut context = manager.into_context();
    context.messages.push(ContextMessage::internal(
        MessageKind::Permission,
        "permission request",
    ));
    context.messages.push(ContextMessage {
        role: MessageRole::Tool,
        content: "orphan".into(),
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: Some("orphan".to_string()),
        tool_name: Some("terminal".to_string()),
        kind: MessageKind::Conversation,
    });
    let mut manager = ContextManager::new(context);
    let plan = manager.plan_compaction(&CompactionPlanner::new(12).with_recent_turns(0));
    assert_eq!(plan.recent_user_messages.len(), 2);
    assert_eq!(plan.recent_user_messages[0].content, "user 2");
    assert!(
        plan.view
            .messages
            .iter()
            .all(|message| message.kind != MessageKind::Permission)
    );
    assert!(
        plan.view
            .messages
            .iter()
            .all(|message| message.reasoning.is_none())
    );
    let assistant = plan
        .view
        .messages
        .iter()
        .find(|message| !message.tool_calls.is_empty())
        .unwrap();
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(assistant.tool_calls[0]["id"], "paired");
    assert_eq!(
        assistant.tool_calls[0]["arguments"]["command"],
        long_command
    );
    let tool_result = plan
        .view
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .unwrap();
    assert_eq!(tool_result.tool_call_id.as_deref(), Some("paired"));
    assert!(tool_result.content.contains("completed"));
    assert!(tool_result.content.contains("content omitted"));
    assert!(!tool_result.content.contains(&"x".repeat(500)));
    assert!(
        !plan
            .view
            .messages
            .iter()
            .any(|message| message.tool_call_id.as_deref() == Some("orphan"))
    );

    manager
        .apply_compaction(plan, "compact summary", &builder)
        .unwrap();
    assert_eq!(manager.context().messages.len(), 4);
    assert_eq!(manager.context().messages[0].role, MessageRole::System);
    assert_eq!(manager.context().messages[1].content, "user 2");
    assert_eq!(manager.context().messages[2].content, "user 3");
    assert_eq!(
        manager.context().messages[3].kind,
        MessageKind::CompactionSummary
    );
    assert_eq!(manager.context().compaction.count, 1);
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 0);
}

#[test]
fn recent_users_use_a_total_utf8_byte_budget_instead_of_a_message_count() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    for index in 0..100 {
        manager.append_user(
            TurnId::parse(format!("turn-{index}")).unwrap(),
            format!("message-{index:03}"),
        );
    }
    let plan = manager.plan_compaction(&CompactionPlanner::default().with_recent_turns(0));
    assert_eq!(plan.recent_user_messages.len(), 100);

    let long = format!("HEAD{}TAIL", "你".repeat(10_000));
    manager.append_user(TurnId::parse("turn-long").unwrap(), long);
    let plan = manager.plan_compaction(&CompactionPlanner::default().with_recent_turns(0));
    let retained = plan.recent_user_messages.last().unwrap();
    assert!(retained.content.starts_with("HEAD"));
    assert!(retained.content.ends_with("TAIL"));
    assert!(retained.content.contains("content omitted"));
    assert!(
        plan.recent_user_messages
            .iter()
            .map(|message| message.content.len())
            .sum::<usize>()
            <= 20_000
    );
}

#[test]
fn acp_content_blocks_round_trip_without_provider_shaping() {
    let value = json!([
        {"type":"text", "text":"describe this", "annotations":{"audience":["user"], "priority":0.5, "_meta":{"source":"test"}}},
        {"type":"image", "mimeType":"image/png", "data":"aGVsbG8=", "uri":"file:///shot.png"},
        {"type":"audio", "mimeType":"audio/wav", "data":"UklGRg=="},
        {"type":"resource", "resource":{"uri":"file:///main.rs", "mimeType":"text/rust", "text":"fn main() {}"}},
        {"type":"resource_link", "uri":"file:///guide.pdf", "name":"guide.pdf", "mimeType":"application/pdf", "size":42}
    ]);
    let content: MessageContent = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(serde_json::to_value(content).unwrap(), value);
}

#[test]
fn recent_user_compaction_caps_text_without_touching_image_data() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    let image_data = "a".repeat(10_000);
    manager.append_user(
        TurnId::parse("turn-image").unwrap(),
        MessageContent::blocks(vec![
            ContentBlock::text(format!("HEAD{}TAIL", "你".repeat(100))),
            ContentBlock::image("image/png", image_data.clone()),
        ]),
    );

    let plan = manager.plan_compaction(&CompactionPlanner::new(80));
    let retained = &plan.recent_turn_messages[0].content;
    assert!(retained.text_bytes() <= 80);
    assert!(retained.contains("content omitted"));
    assert!(matches!(
        &retained.as_blocks()[1],
        ContentBlock::Image { data, .. } if data == &image_data
    ));
}

#[test]
fn compaction_removes_historical_images_but_keeps_latest_three_turn_images() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    for index in 1..=4 {
        let turn = TurnId::parse(format!("turn-{index}")).unwrap();
        manager.append_user(
            turn.clone(),
            MessageContent::blocks(vec![
                ContentBlock::text(format!("user {index}")),
                ContentBlock::image("image/png", format!("image-{index}")),
            ]),
        );
        manager.append_assistant(turn, format!("answer {index}"), Vec::new());
    }

    let plan = manager.plan_compaction(&CompactionPlanner::default());
    let historical_user = plan
        .view
        .messages
        .iter()
        .find(|message| message.content.contains("user 1"))
        .unwrap();
    assert!(
        historical_user
            .content
            .as_blocks()
            .iter()
            .all(|block| !matches!(block, ContentBlock::Image { .. }))
    );
    assert!(
        plan.recent_user_messages[0]
            .content
            .as_blocks()
            .iter()
            .all(|block| !matches!(block, ContentBlock::Image { .. }))
    );

    let latest_images = plan
        .recent_turn_messages
        .iter()
        .filter(|message| message.is_real_user())
        .flat_map(|message| message.content.as_blocks())
        .filter_map(|block| match block {
            ContentBlock::Image { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(latest_images, ["image-2", "image-3", "image-4"]);
}

#[test]
fn image_downgrade_summary_sees_images_and_replacement_removes_them() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    for index in 1..=2 {
        let turn = TurnId::parse(format!("turn-image-{index}")).unwrap();
        manager.append_user(
            turn.clone(),
            MessageContent::blocks(vec![
                ContentBlock::text(format!("user {index}")),
                ContentBlock::image("image/png", format!("image-{index}")),
            ]),
        );
        manager.append_assistant(turn, format!("answer {index}"), Vec::new());
    }

    let plan = manager.plan_image_downgrade();
    let summary_images = plan
        .view
        .messages
        .iter()
        .flat_map(|message| message.content.as_blocks())
        .filter_map(|block| match block {
            ContentBlock::Image { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(summary_images, ["image-1", "image-2"]);
    assert!(plan.recent_turn_messages.is_empty());

    manager
        .apply_compaction(plan, "text description of both images", &builder)
        .unwrap();
    assert!(!manager.contains_images());
    assert_eq!(manager.context().compaction.count, 1);
    assert_eq!(
        manager.context().compaction.summary.as_deref(),
        Some("text description of both images")
    );
}

#[test]
fn historical_and_latest_turn_users_share_one_utf8_byte_budget() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    for index in 1..=5 {
        let turn = TurnId::parse(format!("turn-{index}")).unwrap();
        manager.append_user(
            turn.clone(),
            format!("HEAD-{index}-{}-TAIL-{index}", "x".repeat(5_980)),
        );
        manager.append_assistant(turn, format!("answer {index}"), Vec::new());
    }

    let plan = manager.plan_compaction(&CompactionPlanner::default());
    let retained_user_bytes = plan
        .recent_user_messages
        .iter()
        .chain(
            plan.recent_turn_messages
                .iter()
                .filter(|message| message.is_real_user()),
        )
        .map(|message| message.content.text_bytes())
        .sum::<usize>();

    assert!(retained_user_bytes <= 20_000);
    assert_eq!(plan.recent_turn_messages[0].content.text_bytes(), 5_994);
    assert_eq!(plan.recent_user_messages.len(), 1);
    assert!(
        plan.recent_user_messages[0]
            .content
            .contains("content omitted")
    );
}

#[test]
fn compaction_summarizes_history_and_filters_the_latest_three_turns() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    for index in 1..=2 {
        let turn = TurnId::parse(format!("turn-{index}")).unwrap();
        manager.append_user(turn.clone(), format!("history user {index}"));
        manager.append_assistant(turn, format!("history answer {index}"), Vec::new());
    }
    let tool_turn = TurnId::parse("turn-3").unwrap();
    manager.append_user(tool_turn.clone(), "inspect the project");
    manager.append_assistant_with_reasoning(
        tool_turn.clone(),
        "checking",
        Some("look for TODO markers".to_string()),
        vec![json!({
            "id":"call-1",
            "name":"terminal",
            "arguments":{"action":"run","command":"rg TODO"}
        })],
    );
    manager.append_tool(
        tool_turn,
        ToolResultRecord {
            tool_call_id: "call-1".to_string(),
            tool_name: "terminal".to_string(),
            output: json!({"output":"src/main.rs:1:TODO"}),
        },
    );
    for index in 4..=5 {
        let turn = TurnId::parse(format!("turn-{index}")).unwrap();
        manager.append_user(turn.clone(), format!("recent user {index}"));
        manager.append_assistant(turn, format!("recent answer {index}"), Vec::new());
    }

    let plan = manager.plan_compaction(&CompactionPlanner::default());
    assert!(plan.has_compactable_history());
    assert_eq!(plan.recent_user_messages.len(), 2);
    assert_eq!(plan.recent_turn_messages.len(), 7);
    assert_eq!(plan.recent_turn_messages[0].content, "inspect the project");
    let filtered_tool_call = &plan.recent_turn_messages[1];
    assert_eq!(filtered_tool_call.role, MessageRole::Assistant);
    assert_eq!(
        filtered_tool_call.reasoning.as_deref(),
        Some("look for TODO markers")
    );
    assert!(filtered_tool_call.content.contains("checking"));
    assert_eq!(filtered_tool_call.tool_calls.len(), 1);
    assert_eq!(filtered_tool_call.tool_calls[0]["id"], "call-1");
    assert_eq!(plan.recent_turn_messages[2].role, MessageRole::Tool);
    assert_eq!(
        plan.recent_turn_messages[2].tool_call_id.as_deref(),
        Some("call-1")
    );
    assert!(
        plan.recent_turn_messages[2]
            .content
            .contains("content omitted")
    );
    assert!(!plan.recent_turn_messages[2].content.contains("TODO"));

    manager.apply_compaction(plan, "summary", &builder).unwrap();
    let messages = manager.model_messages();
    assert_eq!(messages[0].role, MessageRole::System);
    assert_eq!(messages[1].content, "history user 1");
    assert_eq!(messages[2].content, "history user 2");
    assert_eq!(messages[3].kind, MessageKind::CompactionSummary);
    assert_eq!(messages[4].content, "inspect the project");
    assert_eq!(messages[5].tool_calls[0]["id"], "call-1");
    assert_eq!(messages[6].role, MessageRole::Tool);
}

#[test]
fn compaction_without_tools_keeps_the_latest_three_turns() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    for index in 1..=4 {
        let turn = TurnId::parse(format!("turn-{index}")).unwrap();
        manager.append_user(turn.clone(), format!("user {index}"));
        manager.append_assistant(turn, format!("answer {index}"), Vec::new());
    }

    let plan = manager.plan_compaction(&CompactionPlanner::default());
    assert!(plan.has_compactable_history());
    assert_eq!(plan.recent_turn_messages.len(), 6);
    assert_eq!(plan.recent_turn_messages[0].content, "user 2");
    manager.apply_compaction(plan, "summary", &builder).unwrap();

    assert_eq!(manager.model_messages().len(), 9);
    assert_eq!(manager.model_messages()[1].content, "user 1");
    assert_eq!(
        manager.model_messages()[2].kind,
        MessageKind::CompactionSummary
    );
    assert_eq!(manager.model_messages()[3].content, "user 2");
    assert_eq!(manager.model_messages()[8].content, "answer 4");
}

#[test]
fn recent_tool_filter_can_reduce_context_without_requesting_a_summary() {
    let root = tempfile::tempdir().unwrap();
    let builder = SystemPromptBuilder::new(None, root.path());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    let turn = TurnId::parse("turn-tools").unwrap();
    manager.append_user(turn.clone(), "inspect");
    manager.append_assistant(
        turn.clone(),
        "",
        vec![json!({
            "id":"call-1",
            "name":"file_edit",
            "arguments":{"patch":"*** Begin Patch\n*** Add File: x\n+x\n*** End Patch"}
        })],
    );
    manager.append_tool(
        turn,
        ToolResultRecord {
            tool_call_id: "call-1".to_string(),
            tool_name: "file_edit".to_string(),
            output: json!({"status":"completed", "changes":[{"path":"x", "kind":"add"}]}),
        },
    );

    let plan = manager.plan_compaction(&CompactionPlanner::default());
    assert!(!plan.has_compactable_history());
    assert!(plan.needs_replacement());
    manager.apply_compaction(plan, "", &builder).unwrap();

    assert_eq!(manager.model_messages().len(), 4);
    assert_eq!(manager.model_messages()[1].content, "inspect");
    assert!(
        manager.model_messages()[2].tool_calls[0]["arguments"]["patch"]
            .as_str()
            .unwrap()
            .contains("file patch omitted")
    );
    assert_eq!(manager.model_messages()[3].role, MessageRole::Tool);
    assert!(manager.model_messages()[3].content.contains("completed"));
    assert_eq!(manager.context().compaction.summary, None);
}

#[test]
fn compaction_rebuild_absorbs_current_rules() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let cwd = root.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    write(&profile.join("resource/prompts/System.md"), "system v1");
    write(&cwd.join("AGENTS.md"), "cwd rules v1");
    let builder = SystemPromptBuilder::new(Some(profile.clone()), cwd.clone());
    let mut manager = ContextManager::initialize(&builder).unwrap();
    manager.append_user(TurnId::parse("turn-1").unwrap(), "hello");

    write(&profile.join("resource/prompts/System.md"), "system v2");
    write(&cwd.join("AGENTS.md"), "cwd rules v2");
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 2);
    let plan = manager.plan_compaction(&CompactionPlanner::default());
    manager.apply_compaction(plan, "summary", &builder).unwrap();

    assert!(manager.system_prompt().contains("system v2"));
    assert!(manager.system_prompt().contains("cwd rules v2"));
    assert!(!manager.system_prompt().contains("cwd rules v1"));
    assert_eq!(manager.refresh_environment(&builder).unwrap(), 0);
}
