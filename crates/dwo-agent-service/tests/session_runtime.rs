mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use dwo_agent_service::{
    AgentService, ConfirmationDecision, ContentBlock, EndpointId, FsSessionRepository,
    MemorySessionRepository, MessageContent, MessageKind, ModelLimits, NewSession, RuntimePhase,
    SessionConfigUpdate, SessionEventPayload, SessionLlmSettings, SessionMode, SessionRepository,
};
use dwo_tools::PolicyConfig;
use serde_json::json;
use support::{ScriptedCompletionStep, ScriptedModelGateway, ScriptedStep, ScriptedSummaryStep};

fn new_session(cwd: &std::path::Path, mode: SessionMode) -> NewSession {
    NewSession {
        id: None,
        parent_session_id: None,
        title: Some("test".to_string()),
        cwd: cwd.to_path_buf(),
        mode,
        llm: SessionLlmSettings::default(),
    }
}

async fn wait_for_turn_end(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<dwo_agent_service::SessionEvent>,
) -> SessionEventPayload {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.expect("event stream closed");
            if matches!(
                event.payload,
                SessionEventPayload::TurnCompleted { .. }
                    | SessionEventPayload::TurnCancelled { .. }
                    | SessionEventPayload::TurnFailed { .. }
            ) {
                return event.payload;
            }
        }
    })
    .await
    .expect("turn did not finish")
}

fn write(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn unnamed_session_gets_model_generated_title() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::with_completions(
        [ScriptedStep::text("done")],
        [ScriptedCompletionStep {
            content: "Title: \"Flaky websocket tests.\"".to_string(),
            input_tokens: 7,
            output_tokens: 3,
        }],
    );
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(NewSession {
            id: None,
            parent_session_id: None,
            title: None,
            cwd: dir.path().to_path_buf(),
            mode: SessionMode::FullAccess,
            llm: SessionLlmSettings::default(),
        })
        .await
        .unwrap();
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();

    agent
        .prompt(
            EndpointId::new(),
            "Please investigate flaky websocket tests",
        )
        .await
        .unwrap();
    let mut titles = Vec::new();
    let mut turn_completed = false;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !titles.iter().any(|title| title == "Flaky websocket tests") || !turn_completed {
            match subscription.events.recv().await.unwrap().payload {
                SessionEventPayload::TitleChanged { title, .. } => titles.push(title),
                SessionEventPayload::TurnCompleted { .. } => turn_completed = true,
                _ => {}
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(titles, ["Flaky websocket tests"]);
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(snapshot.record.info.title, "Flaky websocket tests");
    assert!(snapshot.record.context.usage.current_tokens > 0);
    assert_eq!(
        snapshot.record.context.usage.current_tokens,
        snapshot.usage.used
    );
    assert_eq!(
        service.list().await.unwrap()[0].info.title,
        "Flaky websocket tests"
    );
    let requests = model.completion_requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].selection.model, "scripted-test-model");
    assert!(
        requests[0].messages[0]
            .content
            .contains("concise conversation title")
    );
    assert!(
        requests[0].messages[1]
            .content
            .contains("flaky websocket tests")
    );
}

#[tokio::test]
async fn explicitly_named_session_keeps_its_title() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::text("done")]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();
    agent.prompt(EndpointId::new(), "rename me").await.unwrap();
    wait_for_turn_end(&mut subscription.events).await;

    assert_eq!(
        agent
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record
            .info
            .title,
        "test"
    );
    assert!(model.completion_requests().await.is_empty());
}

#[tokio::test]
async fn empty_persisted_title_is_repaired_from_the_first_user_question() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let repository = Arc::new(FsSessionRepository::new(&sessions).await.unwrap());
    let service = AgentService::new(
        repository.clone(),
        ScriptedModelGateway::new([ScriptedStep::text("done")]),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(&workspace, SessionMode::FullAccess))
        .await
        .unwrap();
    let id = agent.id().clone();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent
        .prompt(EndpointId::new(), "这是一个用于恢复标题的问题")
        .await
        .unwrap();
    wait_for_turn_end(&mut events).await;
    service.shutdown().await;

    let mut persisted = repository.load(&id).await.unwrap().unwrap();
    persisted.info.title = "   ".to_string();
    repository.save(&persisted).await.unwrap();

    let restarted = AgentService::new(
        repository.clone(),
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );
    let listed = restarted.list().await.unwrap();
    assert_eq!(listed[0].info.title, "这是一个用于恢复标题");
    assert_eq!(
        repository.load(&id).await.unwrap().unwrap().info.title,
        "这是一个用于恢复标题"
    );
}

#[tokio::test]
async fn empty_title_without_history_is_filled_by_the_next_user_question() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let repository = Arc::new(FsSessionRepository::new(&sessions).await.unwrap());
    let service = AgentService::new(
        repository.clone(),
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(&workspace, SessionMode::FullAccess))
        .await
        .unwrap();
    let id = agent.id().clone();
    service.shutdown().await;

    let mut persisted = repository.load(&id).await.unwrap().unwrap();
    persisted.info.title.clear();
    repository.save(&persisted).await.unwrap();

    let restarted = AgentService::new(
        repository.clone(),
        ScriptedModelGateway::new([ScriptedStep::text("done")]),
        PolicyConfig::default(),
    );
    let loaded = restarted.load(&id).await.unwrap();
    let mut events = loaded.attach(EndpointId::new()).await.unwrap().events;
    loaded
        .prompt(EndpointId::new(), "please investigate this failure")
        .await
        .unwrap();
    let title = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::TitleChanged { title, .. } =
                events.recv().await.unwrap().payload
            {
                break title;
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(title, "please inv");
    assert_eq!(
        repository.load(&id).await.unwrap().unwrap().info.title,
        title
    );
}

#[tokio::test]
async fn model_tool_model_cycle_is_persisted_in_context() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::tools(
            vec!["editing".to_string()],
            vec![json!({
                "id": "edit-1",
                "name": "file_edit",
                "arguments": {
                    "patch": "*** Begin Patch\n*** Add File: made.txt\n+made\n*** End Patch"
                }
            })],
        ),
        ScriptedStep::text("done"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();

    let prompt = MessageContent::blocks(vec![
        ContentBlock::text("make a file"),
        ContentBlock::image("image/png", "aGVsbG8="),
    ]);
    agent
        .prompt_content(EndpointId::new(), prompt.clone())
        .await
        .unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut subscription.events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));

    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(snapshot.phase, RuntimePhase::Idle);
    assert_eq!(snapshot.record.context.messages.len(), 5);
    assert!(matches!(
        snapshot.transcript.iter().find_map(|event| match &event.payload {
            SessionEventPayload::UserPromptSubmitted { content, .. } => Some(content),
            _ => None,
        }),
        Some(content) if content == &prompt
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("made.txt")).unwrap(),
        "made\n"
    );
    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages.len(), 4);
}

#[tokio::test]
async fn reasoning_and_answer_deltas_are_broadcast_separately() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::reasoning_text("inspect first", "done")]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();
    agent.prompt(EndpointId::new(), "work").await.unwrap();

    let mut reasoning = String::new();
    let mut answer = String::new();
    let mut committed = None;
    loop {
        let event = subscription.events.recv().await.unwrap();
        match event.payload {
            SessionEventPayload::AssistantReasoningDelta { delta, .. } => {
                reasoning.push_str(&delta)
            }
            SessionEventPayload::AssistantDelta { delta, .. } => answer.push_str(&delta),
            SessionEventPayload::AssistantCompleted {
                content,
                reasoning,
                tool_calls,
                ..
            } => committed = Some((content, reasoning, tool_calls)),
            SessionEventPayload::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    assert_eq!(reasoning, "inspect first");
    assert_eq!(answer, "done");
    let (content, committed_reasoning, tool_calls) = committed.unwrap();
    assert_eq!(content, "done");
    assert_eq!(committed_reasoning.as_deref(), Some("inspect first"));
    assert!(tool_calls.is_empty());
}

#[tokio::test]
async fn prompt_is_broadcast_to_observers_but_not_echoed_to_its_origin() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::text("done")]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let origin = EndpointId::new();
    let observer = EndpointId::new();
    let mut origin_events = agent.attach(origin.clone()).await.unwrap().events;
    let mut observer_events = agent.attach(observer).await.unwrap().events;

    let turn_id = agent
        .prompt(origin.clone(), "hello observers")
        .await
        .unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::UserPromptSubmitted {
                turn_id,
                origin,
                content,
            } = observer_events.recv().await.unwrap().payload
            {
                break (turn_id, origin, content);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(observed.0, turn_id);
    assert_eq!(observed.1, origin);
    assert_eq!(observed.2, "hello observers");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = origin_events.recv().await.unwrap();
            assert!(!matches!(
                &event.payload,
                SessionEventPayload::UserPromptSubmitted { .. }
            ));
            if matches!(&event.payload, SessionEventPayload::TurnCompleted { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn prompt_waits_for_the_response_boundary_without_cancelling_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::delayed_text("first answer", 200),
        ScriptedStep::text("replacement"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let endpoint = EndpointId::new();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    let first = agent.prompt(endpoint.clone(), "first").await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        while model.requests().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), agent.prompt(endpoint, "second"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { turn_id } if turn_id == first
    ));

    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == "second")
    );
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(
        snapshot.record.context.messages.last().unwrap().content,
        "replacement"
    );
}

#[tokio::test]
async fn cancel_clears_queued_user_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::delayed_text("late", 5_000)]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    let first = agent.prompt(EndpointId::new(), "first").await.unwrap();
    while model.requests().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queued_agent = agent.clone();
    let queued =
        tokio::spawn(async move { queued_agent.prompt(EndpointId::new(), "queued").await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    agent.cancel(Some(first.clone())).await.unwrap();
    let error = queued.await.unwrap().unwrap_err();
    assert!(matches!(
        error,
        dwo_agent_service::AgentServiceError::PromptCancelled(_)
    ));
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCancelled { turn_id } if turn_id == first
    ));
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(snapshot.transcript.iter().all(|event| {
        !matches!(
            &event.payload,
            SessionEventPayload::UserPromptSubmitted { content, .. }
                if content.as_text() == Some("queued")
        )
    }));
}

#[tokio::test]
async fn queued_prompts_are_delivered_fifo_in_the_same_turn() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::delayed_text("first answer", 200),
        ScriptedStep::text("combined answer"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    let turn = agent.prompt(EndpointId::new(), "first").await.unwrap();
    while model.requests().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let second_agent = agent.clone();
    let second =
        tokio::spawn(async move { second_agent.prompt(EndpointId::new(), "second").await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let third_agent = agent.clone();
    let third = tokio::spawn(async move { third_agent.prompt(EndpointId::new(), "third").await });
    assert_eq!(second.await.unwrap().unwrap(), turn);
    assert_eq!(third.await.unwrap().unwrap(), turn);
    wait_for_turn_end(&mut events).await;

    let requests = model.requests().await;
    let users = requests[1]
        .messages
        .iter()
        .filter(|message| {
            message.role == dwo_agent_service::MessageRole::User
                && message.kind == dwo_agent_service::MessageKind::Conversation
        })
        .map(|message| message.content.to_string())
        .collect::<Vec<_>>();
    assert_eq!(users, ["first", "second", "third"]);
}

#[tokio::test]
async fn cancel_keeps_internal_messages_without_waking_another_step() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::delayed_text("late", 5_000)]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    let turn = agent.prompt(EndpointId::new(), "work").await.unwrap();
    while model.requests().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let notifying_agent = agent.clone();
    let notification = tokio::spawn(async move {
        notifying_agent
            .notify_internal("<env_watcher>changed</env_watcher>")
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    agent.cancel(Some(turn.clone())).await.unwrap();
    assert_eq!(notification.await.unwrap().unwrap(), None);
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCancelled { turn_id } if turn_id == turn
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(model.requests().await.len(), 1);
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(snapshot.record.context.messages.iter().any(|message| {
        message.kind == dwo_agent_service::MessageKind::Runtime
            && message.content.contains("<env_watcher>")
    }));
}

#[tokio::test]
async fn rejected_duplicate_file_edits_keep_calls_and_results_unchanged_before_retry() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::tools(
            Vec::new(),
            vec![
                json!({
                    "id":"edit-a",
                    "name":"file_edit",
                    "arguments":{"patch":"*** Begin Patch\n*** Add File: a\n+a\n*** End Patch"}
                }),
                json!({
                    "id":"edit-b",
                    "name":"file_edit",
                    "arguments":{"patch":"*** Begin Patch\n*** Add File: b\n+b\n*** End Patch"}
                }),
            ],
        ),
        ScriptedStep::text("retry in a later response"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent
        .prompt(EndpointId::new(), "make two files")
        .await
        .unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));

    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    let assistant = requests[1]
        .messages
        .iter()
        .find(|message| !message.tool_calls.is_empty())
        .unwrap();
    assert_eq!(assistant.tool_calls.len(), 2);
    assert!(
        assistant.tool_calls[0]["arguments"]["patch"]
            .as_str()
            .unwrap()
            .contains("*** Add File: a")
    );
    assert!(
        assistant.tool_calls[1]["arguments"]["patch"]
            .as_str()
            .unwrap()
            .contains("*** Add File: b")
    );
    let results = requests[1]
        .messages
        .iter()
        .filter(|message| message.role == dwo_agent_service::MessageRole::Tool)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|result| result.content.contains("multiple_file_edit_calls"))
    );
    assert!(!dir.path().join("a").exists());
    assert!(!dir.path().join("b").exists());
}

#[tokio::test]
async fn internal_message_during_tool_execution_is_injected_after_the_tool_batch() {
    let dir = tempfile::tempdir().unwrap();
    let command = if cfg!(windows) {
        "Start-Sleep -Milliseconds 250; Write-Output done"
    } else {
        "sleep 0.25; printf done"
    };
    let model = ScriptedModelGateway::new([
        ScriptedStep::tools(
            Vec::new(),
            vec![json!({
                "id": "slow-tool",
                "name": "terminal",
                "arguments": {"action": "run", "command": command}
            })],
        ),
        ScriptedStep::text("used child result"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "start work").await.unwrap();

    loop {
        if matches!(
            events.recv().await.unwrap().payload,
            SessionEventPayload::ToolStarted { .. }
        ) {
            break;
        }
    }
    agent
        .notify_internal("<subsession_result session_id=\"child\">ready</subsession_result>")
        .await
        .unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));

    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    let notification_index = requests[1]
        .messages
        .iter()
        .position(|message| {
            message.kind == dwo_agent_service::MessageKind::Runtime
                && message.content.contains("<subsession_result")
        })
        .unwrap();
    let tool_index = requests[1]
        .messages
        .iter()
        .position(|message| message.role == dwo_agent_service::MessageRole::Tool)
        .unwrap();
    assert!(
        notification_index > tool_index,
        "message order: {:?}",
        requests[1]
            .messages
            .iter()
            .map(|message| (&message.role, &message.content))
            .collect::<Vec<_>>()
    );
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(
        snapshot
            .transcript
            .iter()
            .filter(|event| matches!(
                event.payload,
                SessionEventPayload::UserPromptSubmitted { .. }
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn waking_internal_message_starts_an_idle_session_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::text("handled notification")]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent
        .notify_internal("<subsession_result session_id=\"child\">done</subsession_result>")
        .await
        .unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
    let requests = model.requests().await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().any(|message| {
        message.kind == dwo_agent_service::MessageKind::Runtime
            && message.content.contains("<subsession_result")
    }));
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(snapshot.transcript.iter().all(|event| !matches!(
        event.payload,
        SessionEventPayload::UserPromptSubmitted { .. }
    )));
}

#[tokio::test]
async fn usage_trigger_filters_a_recent_tool_turn_without_calling_the_summary_model() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::with_compaction(
        [
            ScriptedStep::Response {
                chunks: vec!["editing".to_string()],
                tool_calls: vec![json!({
                    "id": "edit-compact",
                    "name": "file_edit",
                    "arguments": {
                        "patch": "*** Begin Patch\n*** Add File: compacted.txt\n+done\n*** End Patch"
                    }
                })],
                finish_reason: dwo_agent_service::FinishReason::ToolCalls,
                delay_ms: 0,
                input_tokens: 80,
                output_tokens: 5,
            },
            ScriptedStep::Response {
                chunks: vec!["finished".to_string()],
                tool_calls: Vec::new(),
                finish_reason: dwo_agent_service::FinishReason::Stop,
                delay_ms: 0,
                input_tokens: 20,
                output_tokens: 2,
            },
        ],
        [],
        ModelLimits {
            context_window_tokens: 3_000,
            max_output_tokens: 500,
            max_input_tokens: 2_500,
            compact_trigger_tokens: 2_000,
        },
    );
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent
        .prompt(EndpointId::new(), "make the file")
        .await
        .unwrap();
    let mut usage_updates = Vec::new();
    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.unwrap().payload {
                SessionEventPayload::UsageChanged { used, size } => {
                    usage_updates.push((used, size));
                }
                terminal @ (SessionEventPayload::TurnCompleted { .. }
                | SessionEventPayload::TurnCancelled { .. }
                | SessionEventPayload::TurnFailed { .. }) => break terminal,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        terminal,
        SessionEventPayload::TurnCompleted { .. }
    ));

    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(snapshot.record.context.compaction.count, 1);
    assert!(snapshot.transcript.iter().any(|event| {
        matches!(
            event.payload,
            SessionEventPayload::AssistantCompleted { .. }
        )
    }));
    assert!(snapshot.record.context.usage.current_tokens > 0);
    assert_eq!(
        snapshot.usage.used,
        snapshot.record.context.usage.current_tokens
    );
    assert_eq!(snapshot.usage.size, 3_000);
    assert!(
        usage_updates
            .iter()
            .all(|(used, size)| *used > 0 && *size == 3_000)
    );
    assert!(
        usage_updates
            .windows(2)
            .any(|updates| updates[1].0 < updates[0].0),
        "tool filtering should reduce the estimated context: {usage_updates:?}"
    );
    assert_eq!(model.summary_request_count().await, 0);
    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages[0].role,
        dwo_agent_service::MessageRole::System
    );
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|message| message.kind == MessageKind::CompactionSummary)
    );
    let assistant = requests[1]
        .messages
        .iter()
        .find(|message| !message.tool_calls.is_empty())
        .unwrap();
    assert!(
        assistant.tool_calls[0]["arguments"]["patch"]
            .as_str()
            .unwrap()
            .contains("file patch omitted")
    );
    let tool_result = requests[1]
        .messages
        .iter()
        .find(|message| message.role == dwo_agent_service::MessageRole::Tool)
        .unwrap();
    assert_eq!(tool_result.tool_call_id.as_deref(), Some("edit-compact"));
    assert!(tool_result.content.contains("completed"));
}

#[tokio::test]
async fn context_error_summarizes_old_history_and_retries_with_filtered_recent_turns() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::with_compaction(
        [
            ScriptedStep::text("answer 1"),
            ScriptedStep::text("answer 2"),
            ScriptedStep::text("answer 3"),
            ScriptedStep::text("answer 4"),
            ScriptedStep::tools(
                vec!["checking".to_string()],
                vec![json!({
                    "id": "edit-recovery",
                    "name": "file_edit",
                    "arguments": {
                        "patch": "*** Begin Patch\n*** Add File: recovery.txt\n+done\n*** End Patch"
                    }
                })],
            ),
            ScriptedStep::ContextLengthExceeded,
            ScriptedStep::text("recovered"),
        ],
        [ScriptedSummaryStep {
            summary: "older work summary".to_string(),
            input_tokens: 40,
            output_tokens: 8,
        }],
        ModelLimits {
            context_window_tokens: u64::MAX,
            max_output_tokens: u32::MAX,
            max_input_tokens: u64::MAX,
            compact_trigger_tokens: u64::MAX,
        },
    );
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    for index in 1..=4 {
        agent
            .prompt(EndpointId::new(), format!("request {index}"))
            .await
            .unwrap();
        assert!(matches!(
            wait_for_turn_end(&mut events).await,
            SessionEventPayload::TurnCompleted { .. }
        ));
    }
    agent
        .prompt(EndpointId::new(), "run the edit")
        .await
        .unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));

    let requests = model.requests().await;
    assert_eq!(requests.len(), 7);
    assert!(
        requests[5]
            .messages
            .iter()
            .any(|message| message.role == dwo_agent_service::MessageRole::Tool)
    );
    let compacted_assistant = requests[6]
        .messages
        .iter()
        .find(|message| !message.tool_calls.is_empty())
        .unwrap();
    assert_eq!(compacted_assistant.tool_calls[0]["id"], "edit-recovery");
    assert!(
        compacted_assistant.tool_calls[0]["arguments"]["patch"]
            .as_str()
            .unwrap()
            .contains("file patch omitted")
    );
    let compacted_result = requests[6]
        .messages
        .iter()
        .find(|message| message.role == dwo_agent_service::MessageRole::Tool)
        .unwrap();
    assert_eq!(
        compacted_result.tool_call_id.as_deref(),
        Some("edit-recovery")
    );
    assert!(compacted_result.content.contains("completed"));
    assert!(
        requests[6]
            .messages
            .iter()
            .any(|message| message.kind == MessageKind::CompactionSummary)
    );

    let summaries = model.summary_requests().await;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].selection.model, "scripted-test-model");
    assert!(
        summaries[0]
            .view
            .messages
            .iter()
            .any(|message| message.content == "request 1")
    );
    assert!(
        !summaries[0]
            .view
            .messages
            .iter()
            .any(|message| message.content == "run the edit")
    );
}

#[tokio::test]
async fn switching_models_compacts_with_the_last_successful_model_and_keeps_reasoning() {
    let dir = tempfile::tempdir().unwrap();
    let unlimited = ModelLimits {
        context_window_tokens: u64::MAX,
        max_output_tokens: u32::MAX,
        max_input_tokens: u64::MAX,
        compact_trigger_tokens: u64::MAX,
    };
    let model = ScriptedModelGateway::with_model_limits(
        [
            ScriptedStep::Response {
                chunks: vec!["answer 1".to_string()],
                tool_calls: Vec::new(),
                finish_reason: dwo_agent_service::FinishReason::Stop,
                delay_ms: 0,
                input_tokens: 10,
                output_tokens: 1,
            },
            ScriptedStep::Response {
                chunks: vec!["answer 2".to_string()],
                tool_calls: Vec::new(),
                finish_reason: dwo_agent_service::FinishReason::Stop,
                delay_ms: 0,
                input_tokens: 10,
                output_tokens: 1,
            },
            ScriptedStep::Response {
                chunks: vec!["answer 3".to_string()],
                tool_calls: Vec::new(),
                finish_reason: dwo_agent_service::FinishReason::Stop,
                delay_ms: 0,
                input_tokens: 10,
                output_tokens: 1,
            },
            ScriptedStep::Response {
                chunks: vec!["answer 4".to_string()],
                tool_calls: Vec::new(),
                finish_reason: dwo_agent_service::FinishReason::Stop,
                delay_ms: 0,
                input_tokens: 400,
                output_tokens: 1,
            },
            ScriptedStep::text("new model answer"),
        ],
        [ScriptedSummaryStep {
            summary: "summary from old model".to_string(),
            input_tokens: 30,
            output_tokens: 5,
        }],
        [
            ("scripted-test-model".to_string(), unlimited),
            (
                "next-model".to_string(),
                ModelLimits {
                    context_window_tokens: 60_000,
                    max_output_tokens: 10_000,
                    max_input_tokens: 50_000,
                    compact_trigger_tokens: 25_000,
                },
            ),
        ],
    );
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    for index in 1..=4 {
        agent
            .prompt(
                EndpointId::new(),
                format!("request {index} {}", "x".repeat(25_000)),
            )
            .await
            .unwrap();
        wait_for_turn_end(&mut events).await;
    }
    agent
        .set_config(SessionConfigUpdate::Reasoning(Some("high".to_string())))
        .await
        .unwrap();
    agent
        .set_config(SessionConfigUpdate::Model("next-model".to_string()))
        .await
        .unwrap();
    let switched_usage = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::UsageChanged { used, size } =
                events.recv().await.unwrap().payload
            {
                break (used, size);
            }
        }
    })
    .await
    .unwrap();
    assert!(switched_usage.0 >= 25_000);
    assert_eq!(switched_usage.1, 60_000);
    agent.prompt(EndpointId::new(), "request 5").await.unwrap();
    let mut compacted_usage = Vec::new();
    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.unwrap().payload {
                SessionEventPayload::UsageChanged { used, size } => {
                    compacted_usage.push((used, size));
                }
                terminal @ (SessionEventPayload::TurnCompleted { .. }
                | SessionEventPayload::TurnCancelled { .. }
                | SessionEventPayload::TurnFailed { .. }) => break terminal,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        terminal,
        SessionEventPayload::TurnCompleted { .. }
    ));
    assert!(
        compacted_usage
            .iter()
            .any(|(used, size)| *used < switched_usage.0 && *size == 60_000),
        "compaction should reduce the estimated context: {compacted_usage:?}"
    );

    let summaries = model.summary_requests().await;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].selection.model, "scripted-test-model");
    assert_eq!(summaries[0].selection.reasoning.as_deref(), Some("high"));
    let requests = model.requests().await;
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[4].selection.model, "next-model");
    assert_eq!(requests[4].selection.reasoning.as_deref(), Some("high"));
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(snapshot.record.llm.reasoning.as_deref(), Some("high"));
    assert!(snapshot.record.context.usage.current_tokens > 0);
    assert_eq!(
        snapshot.usage.used,
        snapshot.record.context.usage.current_tokens
    );
    assert_eq!(snapshot.usage.size, 60_000);
    assert_eq!(
        snapshot.record.context.usage.last_model.as_deref(),
        Some("next-model")
    );
}

#[tokio::test]
async fn switching_to_text_model_summarizes_images_and_preserves_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let unlimited = ModelLimits {
        context_window_tokens: u64::MAX,
        max_output_tokens: u32::MAX,
        max_input_tokens: u64::MAX,
        compact_trigger_tokens: u64::MAX,
    };
    let model = ScriptedModelGateway::with_model_limits_and_capabilities(
        [
            ScriptedStep::text("vision answer"),
            ScriptedStep::text("text answer"),
        ],
        [ScriptedSummaryStep {
            summary: "the screenshot shows a compiler error".to_string(),
            input_tokens: 30,
            output_tokens: 8,
        }],
        [
            ("scripted-test-model".to_string(), unlimited),
            (
                "text-model".to_string(),
                ModelLimits {
                    context_window_tokens: 5_000,
                    max_output_tokens: 1_000,
                    max_input_tokens: 4_000,
                    compact_trigger_tokens: 3_000,
                },
            ),
        ],
        [
            ("scripted-test-model".to_string(), true),
            ("text-model".to_string(), false),
        ],
    );
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();
    let image_prompt = MessageContent::blocks(vec![
        ContentBlock::text("explain this error"),
        ContentBlock::image("image/png", "aGVsbG8="),
    ]);
    agent
        .prompt_content(EndpointId::new(), image_prompt.clone())
        .await
        .unwrap();
    wait_for_turn_end(&mut subscription.events).await;

    agent
        .set_config(SessionConfigUpdate::Model("text-model".to_string()))
        .await
        .unwrap();
    let migrated_usage = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::UsageChanged { used, size } =
                subscription.events.recv().await.unwrap().payload
            {
                break (used, size);
            }
        }
    })
    .await
    .unwrap();
    assert!(migrated_usage.0 > 0);
    assert_eq!(migrated_usage.1, 5_000);

    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(snapshot.record.llm.model, "text-model");
    assert_eq!(
        snapshot.record.context.usage.current_tokens,
        migrated_usage.0
    );
    assert!(
        snapshot
            .record
            .context
            .messages
            .iter()
            .all(|message| !message.content.contains_images())
    );
    assert_eq!(
        snapshot.record.context.compaction.summary.as_deref(),
        Some("the screenshot shows a compiler error")
    );
    assert!(matches!(
        snapshot.transcript.iter().find_map(|event| match &event.payload {
            SessionEventPayload::UserPromptSubmitted { content, .. }
                if content.contains_images() => Some(content),
            _ => None,
        }),
        Some(content) if content == &image_prompt
    ));
    let summaries = model.summary_requests().await;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].selection.model, "scripted-test-model");
    assert!(
        summaries[0]
            .view
            .messages
            .iter()
            .any(|message| message.content.contains_images())
    );

    agent.prompt(EndpointId::new(), "continue").await.unwrap();
    wait_for_turn_end(&mut subscription.events).await;
    let requests = model.requests().await;
    assert_eq!(requests[1].selection.model, "text-model");
    assert!(
        requests[1]
            .messages
            .iter()
            .all(|message| !message.content.contains_images())
    );
}

#[tokio::test]
async fn failed_image_summary_leaves_model_and_context_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let limits = ModelLimits {
        context_window_tokens: 1_000,
        max_output_tokens: 100,
        max_input_tokens: 900,
        compact_trigger_tokens: 800,
    };
    let model = ScriptedModelGateway::with_model_limits_and_capabilities(
        [ScriptedStep::text("vision answer")],
        [],
        [
            ("scripted-test-model".to_string(), limits),
            ("text-model".to_string(), limits),
        ],
        [
            ("scripted-test-model".to_string(), true),
            ("text-model".to_string(), false),
        ],
    );
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();
    agent
        .prompt_content(
            EndpointId::new(),
            MessageContent::blocks(vec![ContentBlock::image("image/png", "aGVsbG8=")]),
        )
        .await
        .unwrap();
    wait_for_turn_end(&mut subscription.events).await;
    let before = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    let before_context = serde_json::to_value(&before.record.context).unwrap();

    let error = agent
        .set_config(SessionConfigUpdate::Model("text-model".to_string()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("prepare image context"));
    let after = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(after.record.llm.model, "scripted-test-model");
    assert_eq!(
        serde_json::to_value(&after.record.context).unwrap(),
        before_context
    );
}

#[tokio::test]
async fn text_model_switch_is_rejected_while_an_image_turn_is_active() {
    let dir = tempfile::tempdir().unwrap();
    let limits = ModelLimits {
        context_window_tokens: 1_000,
        max_output_tokens: 100,
        max_input_tokens: 900,
        compact_trigger_tokens: 800,
    };
    let model = ScriptedModelGateway::with_model_limits_and_capabilities(
        [ScriptedStep::delayed_text("late", 5_000)],
        [],
        [
            ("scripted-test-model".to_string(), limits),
            ("text-model".to_string(), limits),
        ],
        [
            ("scripted-test-model".to_string(), true),
            ("text-model".to_string(), false),
        ],
    );
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();
    let turn_id = agent
        .prompt_content(
            EndpointId::new(),
            MessageContent::blocks(vec![ContentBlock::image("image/png", "aGVsbG8=")]),
        )
        .await
        .unwrap();

    let error = agent
        .set_config(SessionConfigUpdate::Model("text-model".to_string()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("image turn is active"));
    assert_eq!(
        agent
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record
            .llm
            .model,
        "scripted-test-model"
    );
    agent.cancel(Some(turn_id)).await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut subscription.events).await,
        SessionEventPayload::TurnCancelled { .. }
    ));
}

#[tokio::test]
async fn text_model_rejects_new_images_before_persisting_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let limits = ModelLimits {
        context_window_tokens: 1_000,
        max_output_tokens: 100,
        max_input_tokens: 900,
        compact_trigger_tokens: 800,
    };
    let model = ScriptedModelGateway::with_model_limits_and_capabilities(
        [],
        [],
        [("text-model".to_string(), limits)],
        [("text-model".to_string(), false)],
    );
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let mut session = new_session(dir.path(), SessionMode::FullAccess);
    session.llm.model = "text-model".to_string();
    let agent = service.create(session).await.unwrap();
    let error = agent
        .prompt_content(
            EndpointId::new(),
            MessageContent::blocks(vec![ContentBlock::image("image/png", "aGVsbG8=")]),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("does not support image input"));
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(snapshot.transcript.is_empty());
    assert!(
        snapshot
            .record
            .context
            .messages
            .iter()
            .all(|message| !message.content.contains_images())
    );
}

#[tokio::test]
async fn prompt_is_stable_while_agents_changes_are_appended_as_watcher_messages() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("workspace");
    let profile = root.path().join("profile");
    std::fs::create_dir_all(&cwd).unwrap();
    write(
        &profile.join("resource/prompts/System.md"),
        "profile prompt",
    );
    write(
        &profile.join("resource/prompts/AGENTS.md"),
        "profile rules v1",
    );
    write(&cwd.join("AGENTS.md"), "cwd rules v1");
    let model = ScriptedModelGateway::new([
        ScriptedStep::text("first response"),
        ScriptedStep::text("second response"),
    ]);
    let service = AgentService::with_profile_root(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
        profile.clone(),
    )
    .unwrap();
    let agent = service
        .create(NewSession {
            id: None,
            parent_session_id: None,
            title: Some("profile test".to_string()),
            cwd: cwd.clone(),
            mode: SessionMode::FullAccess,
            llm: SessionLlmSettings::default(),
        })
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "first").await.unwrap();
    wait_for_turn_end(&mut events).await;

    write(
        &profile.join("resource/prompts/AGENTS.md"),
        "profile rules v2",
    );
    write(&cwd.join("AGENTS.md"), "cwd rules v2");
    agent.prompt(EndpointId::new(), "second").await.unwrap();
    wait_for_turn_end(&mut events).await;

    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0].messages[0].content.contains("profile prompt"));
    assert!(requests[0].messages[0].content.contains("profile rules v1"));
    assert!(requests[0].messages[0].content.contains("cwd rules v1"));
    assert_eq!(requests[0].messages[0], requests[1].messages[0]);
    let watcher = requests[1]
        .messages
        .iter()
        .find(|message| message.kind == dwo_agent_service::MessageKind::EnvWatcher)
        .unwrap();
    assert!(watcher.content.contains("profile rules v2"));
    assert!(watcher.content.contains("cwd rules v2"));
}

#[tokio::test]
async fn attach_while_running_returns_partial_state_then_gapless_live_events() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::Response {
        chunks: vec!["hel".to_string(), "lo".to_string()],
        tool_calls: Vec::new(),
        finish_reason: dwo_agent_service::FinishReason::Stop,
        delay_ms: 100,
        input_tokens: 0,
        output_tokens: 0,
    }]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut first = agent.attach(EndpointId::new()).await.unwrap();
    agent.prompt(EndpointId::new(), "hello").await.unwrap();

    let first_delta_seq = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = first.events.recv().await.unwrap();
            if matches!(event.payload, SessionEventPayload::AssistantDelta { .. }) {
                break event.seq;
            }
        }
    })
    .await
    .unwrap();
    let mut attached = agent.attach(EndpointId::new()).await.unwrap();
    assert_eq!(attached.snapshot.phase, RuntimePhase::Running);
    assert_eq!(attached.snapshot.partial_message, "hel");
    assert!(attached.snapshot.seq >= first_delta_seq);

    let next = attached.events.recv().await.unwrap();
    assert!(next.seq > attached.snapshot.seq);
    assert!(matches!(
        wait_for_turn_end(&mut attached.events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
}

#[tokio::test]
async fn another_endpoint_can_cancel_the_current_turn() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::delayed_text("late", 5_000)]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();
    let turn_id = agent.prompt(EndpointId::new(), "wait").await.unwrap();
    agent.cancel(Some(turn_id)).await.unwrap();

    assert!(matches!(
        wait_for_turn_end(&mut subscription.events).await,
        SessionEventPayload::TurnCancelled { .. }
    ));
    assert_eq!(
        agent
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .phase,
        RuntimePhase::Idle
    );
}

#[tokio::test]
async fn sessions_run_in_parallel() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::delayed_text("one", 300),
        ScriptedStep::delayed_text("two", 300),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let first = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let second = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut first_events = first.attach(EndpointId::new()).await.unwrap().events;
    let mut second_events = second.attach(EndpointId::new()).await.unwrap().events;

    let started = Instant::now();
    let (first_prompt, second_prompt) = tokio::join!(
        first.prompt(EndpointId::new(), "one"),
        second.prompt(EndpointId::new(), "two")
    );
    first_prompt.unwrap();
    second_prompt.unwrap();
    tokio::join!(
        wait_for_turn_end(&mut first_events),
        wait_for_turn_end(&mut second_events)
    );
    assert!(started.elapsed() < Duration::from_millis(550));
}

#[tokio::test]
async fn mode_change_applies_to_the_next_tool_batch() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::Response {
            chunks: vec!["attempt".to_string()],
            tool_calls: vec![json!({
                "id": "edit-1",
                "name": "file_edit",
                "arguments": {
                    "patch": "*** Begin Patch\n*** Add File: denied.txt\n+no\n*** End Patch"
                }
            })],
            finish_reason: dwo_agent_service::FinishReason::ToolCalls,
            delay_ms: 150,
            input_tokens: 0,
            output_tokens: 0,
        },
        ScriptedStep::text("observed"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "try").await.unwrap();
    agent
        .set_config(SessionConfigUpdate::Mode(SessionMode::Watch))
        .await
        .unwrap();
    wait_for_turn_end(&mut events).await;

    assert!(!dir.path().join("denied.txt").exists());
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    let tool_output = &snapshot.record.context.messages[3].content;
    assert!(tool_output.contains("blocked_by_policy"), "{tool_output}");
}

#[tokio::test]
async fn any_attached_endpoint_can_answer_permission_and_first_response_wins() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::tools(
            Vec::new(),
            vec![json!({
                "id": "edit-1",
                "name": "file_edit",
                "arguments": {
                    "patch": "*** Begin Patch\n*** Add File: approved.txt\n+yes\n*** End Patch"
                }
            })],
        ),
        ScriptedStep::text("done"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::Confirm))
        .await
        .unwrap();
    let origin = EndpointId::new();
    let responder = EndpointId::new();
    let mut subscription = agent.attach(responder.clone()).await.unwrap();
    agent.prompt(origin.clone(), "edit").await.unwrap();
    let permission = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = subscription.events.recv().await.unwrap();
            if let SessionEventPayload::PermissionRequested { permission, .. } = event.payload {
                break permission;
            }
        }
    })
    .await
    .unwrap();
    let waiting = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(waiting.phase, RuntimePhase::WaitingPermission);
    assert_eq!(waiting.record.context.messages.len(), 2);
    assert!(
        waiting
            .record
            .context
            .messages
            .iter()
            .all(|message| message.tool_calls.is_empty())
    );
    let request_id = permission.request_id;
    agent
        .respond_permission(
            responder.clone(),
            request_id.clone(),
            ConfirmationDecision {
                allowed: true,
                reason: None,
            },
        )
        .await
        .unwrap();
    assert!(
        agent
            .respond_permission(
                origin,
                request_id.clone(),
                ConfirmationDecision {
                    allowed: false,
                    reason: Some("too late".to_string()),
                },
            )
            .await
            .is_err()
    );
    let resolved = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::PermissionResolved {
                request_id: resolved_id,
                responder: resolved_by,
                allowed,
                ..
            } = subscription.events.recv().await.unwrap().payload
            {
                break (resolved_id, resolved_by, allowed);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(resolved.0, request_id);
    assert_eq!(resolved.1, responder);
    assert!(resolved.2);
    assert!(matches!(
        wait_for_turn_end(&mut subscription.events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
    assert!(dir.path().join("approved.txt").exists());
}

#[tokio::test]
async fn origin_disconnect_keeps_permission_available_to_other_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::tools(
            Vec::new(),
            vec![json!({
                "id": "edit-1",
                "name": "file_edit",
                "arguments": {
                    "patch": "*** Begin Patch\n*** Add File: disconnected.txt\n+no\n*** End Patch"
                }
            })],
        ),
        ScriptedStep::text("continued"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::Confirm))
        .await
        .unwrap();
    let origin = EndpointId::new();
    let responder = EndpointId::new();
    let origin_subscription = agent.attach(origin.clone()).await.unwrap();
    let mut events = agent.attach(responder.clone()).await.unwrap().events;
    agent.prompt(origin.clone(), "edit").await.unwrap();
    let permission = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::PermissionRequested { permission, .. } =
                events.recv().await.unwrap().payload
            {
                break permission;
            }
        }
    })
    .await
    .unwrap();

    drop(origin_subscription);
    let waiting = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(waiting.phase, RuntimePhase::WaitingPermission);
    assert_eq!(
        waiting
            .pending_permission
            .as_ref()
            .map(|pending| pending.request_id.as_str()),
        Some(permission.request_id.as_str())
    );
    agent
        .respond_permission(
            responder,
            permission.request_id,
            ConfirmationDecision {
                allowed: false,
                reason: Some("observer denied".to_string()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(snapshot.pending_permission.is_none());
    assert_eq!(
        snapshot.record.context.messages.last().unwrap().content,
        "continued"
    );
    assert!(!dir.path().join("disconnected.txt").exists());
}

#[tokio::test]
async fn concurrent_tools_expose_permission_requests_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::tools(
            Vec::new(),
            vec![
                json!({
                    "id": "terminal-1",
                    "name": "terminal",
                    "arguments": {"action": "run", "command": "echo one"}
                }),
                json!({
                    "id": "terminal-2",
                    "name": "terminal",
                    "arguments": {"action": "run", "command": "echo two"}
                }),
            ],
        ),
        ScriptedStep::text("done"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::Confirm))
        .await
        .unwrap();
    let origin = EndpointId::new();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(origin.clone(), "run both").await.unwrap();

    let first = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::PermissionRequested { permission, .. } =
                events.recv().await.unwrap().payload
            {
                break permission;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err(),
        "a second permission became visible before the first was answered"
    );
    agent
        .respond_permission(
            origin.clone(),
            first.request_id,
            ConfirmationDecision {
                allowed: false,
                reason: Some("test".to_string()),
            },
        )
        .await
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::PermissionRequested { permission, .. } =
                events.recv().await.unwrap().payload
            {
                break permission;
            }
        }
    })
    .await
    .unwrap();
    agent
        .respond_permission(
            origin,
            second.request_id,
            ConfirmationDecision {
                allowed: false,
                reason: Some("test".to_string()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
}

#[tokio::test]
async fn close_waits_for_cancelled_tool_results_before_unloading() {
    let dir = tempfile::tempdir().unwrap();
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 30"
    } else {
        "sleep 30"
    };
    let model = ScriptedModelGateway::new([ScriptedStep::tools(
        Vec::new(),
        vec![json!({
            "id": "terminal-1",
            "name": "terminal",
            "arguments": {
                "action": "run",
                "command": command,
                "yield_ms": 10000
            }
        })],
    )]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let id = agent.id().clone();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "run").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                events.recv().await.unwrap().payload,
                SessionEventPayload::ToolStarted { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();

    service.close(&id).await.unwrap();
    let loaded = service.load(&id).await.unwrap();
    let snapshot = loaded.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(matches!(
        snapshot.transcript.iter().rev().find_map(|event| match &event.payload {
            SessionEventPayload::ToolCompleted { result, .. } => Some(result),
            _ => None,
        }),
        Some(result) if result.output["status"] == "cancelled"
    ));
}

#[tokio::test]
async fn filesystem_repository_loads_context_after_service_restart() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let repository = Arc::new(FsSessionRepository::new(&sessions).await.unwrap());
    let service = AgentService::new(
        repository,
        ScriptedModelGateway::new([
            ScriptedStep::tools(
                vec!["checking".to_string()],
                vec![json!({
                    "id": "persisted-tool",
                    "name": "file_edit",
                    "arguments": {
                        "patch": "*** Begin Patch\n*** Add File: persisted.txt\n+done\n*** End Patch"
                    }
                })],
            ),
            ScriptedStep::reasoning_text("thinking", "persisted"),
        ]),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(&workspace, SessionMode::FullAccess))
        .await
        .unwrap();
    let id = agent.id().clone();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "remember").await.unwrap();
    wait_for_turn_end(&mut events).await;
    service.shutdown().await;

    let restarted = AgentService::new(
        Arc::new(FsSessionRepository::new(&sessions).await.unwrap()),
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );
    let loaded = restarted.load(&id).await.unwrap();
    let snapshot = loaded.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(snapshot.record.context.messages.len(), 5);
    assert_eq!(snapshot.record.context.messages[4].content, "persisted");
    let transcript_kinds = snapshot
        .transcript
        .iter()
        .map(|event| match &event.payload {
            SessionEventPayload::UserPromptSubmitted { .. } => "user",
            SessionEventPayload::AssistantDelta { .. } => "assistant_delta",
            SessionEventPayload::AssistantReasoningDelta { .. } => "reasoning_delta",
            SessionEventPayload::AssistantCompleted { .. } => "assistant_completed",
            SessionEventPayload::ToolStarted { .. } => "tool_started",
            SessionEventPayload::ToolCompleted { .. } => "tool_completed",
            _ => "other",
        })
        .collect::<Vec<_>>();
    assert_eq!(
        transcript_kinds,
        [
            "user",
            "assistant_delta",
            "assistant_completed",
            "tool_started",
            "tool_completed",
            "reasoning_delta",
            "assistant_delta",
            "assistant_completed",
        ]
    );
    assert_eq!(snapshot.phase, RuntimePhase::Idle);
}

#[tokio::test]
async fn delete_closes_a_running_session_and_removes_its_record() {
    let dir = tempfile::tempdir().unwrap();
    let repository = Arc::new(MemorySessionRepository::default());
    let service = AgentService::new(
        repository,
        ScriptedModelGateway::new([ScriptedStep::delayed_text("late", 5_000)]),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::FullAccess))
        .await
        .unwrap();
    let id = agent.id().clone();
    agent.prompt(EndpointId::new(), "wait").await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), service.delete(&id))
        .await
        .expect("delete did not cancel the running session")
        .unwrap();
    assert!(service.list().await.unwrap().is_empty());
    assert!(service.load(&id).await.is_err());
    assert!(service.delete(&id).await.is_err());
    assert!(agent.attach(EndpointId::new()).await.is_err());
}

#[tokio::test]
async fn filesystem_delete_survives_service_restart() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let service = AgentService::new(
        Arc::new(FsSessionRepository::new(&sessions).await.unwrap()),
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(&workspace, SessionMode::FullAccess))
        .await
        .unwrap();
    let id = agent.id().clone();
    service.delete(&id).await.unwrap();

    let restarted = AgentService::new(
        Arc::new(FsSessionRepository::new(&sessions).await.unwrap()),
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );
    assert!(restarted.list().await.unwrap().is_empty());
    assert!(restarted.load(&id).await.is_err());
}

#[tokio::test]
async fn unified_config_persists_mode_model_and_reasoning() {
    let dir = tempfile::tempdir().unwrap();
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::Confirm))
        .await
        .unwrap();
    let id = agent.id().clone();

    service
        .set_config(
            &id,
            SessionConfigUpdate::Reasoning(Some("high".to_string())),
        )
        .await
        .unwrap();
    service
        .set_config(&id, SessionConfigUpdate::Model("next-model".to_string()))
        .await
        .unwrap();
    service
        .set_config(&id, SessionConfigUpdate::Mode(SessionMode::Watch))
        .await
        .unwrap();
    service.close(&id).await.unwrap();

    let config = service
        .load(&id)
        .await
        .unwrap()
        .attach(EndpointId::new())
        .await
        .unwrap()
        .snapshot
        .record
        .config();
    assert_eq!(config.mode, SessionMode::Watch);
    assert_eq!(config.model, "next-model");
    assert_eq!(config.reasoning.as_deref(), Some("high"));
}

#[tokio::test]
async fn invalid_config_is_rejected_without_mutating_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::Confirm))
        .await
        .unwrap();
    assert!(
        agent
            .set_config(SessionConfigUpdate::Model("  ".to_string()))
            .await
            .is_err()
    );
    assert!(
        agent
            .set_config(SessionConfigUpdate::Reasoning(Some(" ".to_string())))
            .await
            .is_err()
    );
    let config = agent
        .attach(EndpointId::new())
        .await
        .unwrap()
        .snapshot
        .record
        .config();
    assert_eq!(config.model, "scripted-test-model");
    assert_eq!(config.reasoning, None);
    assert_eq!(config.mode, SessionMode::Confirm);
}

#[tokio::test]
async fn model_and_reasoning_changes_apply_to_the_next_model_step() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::tools(
            Vec::new(),
            vec![json!({
                "id": "edit-1",
                "name": "file_edit",
                "arguments": {
                    "patch": "*** Begin Patch\n*** Add File: config.txt\n+changed\n*** End Patch"
                }
            })],
        ),
        ScriptedStep::text("done"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model.clone(),
        PolicyConfig::default(),
    );
    let agent = service
        .create(new_session(dir.path(), SessionMode::Confirm))
        .await
        .unwrap();
    let origin = EndpointId::new();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(origin.clone(), "edit").await.unwrap();
    let permission = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::PermissionRequested { permission, .. } =
                events.recv().await.unwrap().payload
            {
                break permission;
            }
        }
    })
    .await
    .unwrap();

    agent
        .set_config(SessionConfigUpdate::Model("next-model".to_string()))
        .await
        .unwrap();
    agent
        .set_config(SessionConfigUpdate::Reasoning(Some("high".to_string())))
        .await
        .unwrap();
    agent
        .respond_permission(
            origin,
            permission.request_id,
            ConfirmationDecision {
                allowed: true,
                reason: None,
            },
        )
        .await
        .unwrap();
    wait_for_turn_end(&mut events).await;

    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].selection.model, "scripted-test-model");
    assert_eq!(requests[0].selection.reasoning, None);
    assert_eq!(requests[1].selection.model, "next-model");
    assert_eq!(requests[1].selection.reasoning.as_deref(), Some("high"));
}
