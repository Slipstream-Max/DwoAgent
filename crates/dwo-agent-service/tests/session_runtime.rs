mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use dwo_agent_service::{
    AgentService, AgentServiceError, ConfirmationDecision, ContentBlock, ContextMessage,
    EndpointId, FinishReason, FsSessionRepository, MemorySessionRepository, MessageContent,
    MessageKind, ModelLimits, ModelReply, ModelStreamEvent, ModelUsage, NewSession, RuntimePhase,
    SessionConfigUpdate, SessionEventPayload, SessionId, SessionLlmSettings, SessionMode,
    SessionRepository, StreamToolCall,
};
use dwo_tools::PolicyConfig;
use serde_json::{Value, json};
use support::{ScriptedCompletionStep, ScriptedModelGateway, ScriptedStep, ScriptedSummaryStep};

fn new_session(cwd: &std::path::Path, mode: SessionMode) -> NewSession {
    NewSession {
        id: None,
        parent_session_id: None,
        title: Some("test".to_string()),
        automation_job: None,
        cwd: cwd.to_path_buf(),
        rule_sources: Vec::new(),
        mode,
        llm: SessionLlmSettings::default(),
        ephemeral: false,
    }
}

async fn wait_for_turn_end(
    events: &mut tokio::sync::mpsc::Receiver<dwo_agent_service::SessionEvent>,
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

fn plan_step(content: &str, status: &str) -> ScriptedStep {
    ScriptedStep::tools(
        Vec::new(),
        vec![json!({
            "id":"plan-call",
            "name":"plan",
            "arguments":{"action":"update", "entries":[{
                "content":content, "priority":"high", "status":status
            }]}
        })],
    )
}

fn ephemeral_session(cwd: &std::path::Path) -> NewSession {
    NewSession {
        ephemeral: true,
        ..new_session(cwd, SessionMode::FullAccess)
    }
}

#[tokio::test]
async fn unfinished_plan_waits_idle_and_reaches_only_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        plan_step("finish plan support", "in_progress"),
        ScriptedStep::text("first turn done"),
        ScriptedStep::text("second turn done"),
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

    agent.prompt(EndpointId::new(), "start").await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
    let snapshot = agent.snapshot().await.unwrap();
    assert_eq!(snapshot.phase, RuntimePhase::Idle);
    assert!(snapshot.active_turn_id.is_none());
    assert!(snapshot.record.current_plan.is_some());
    assert_eq!(
        snapshot
            .record
            .context
            .messages
            .iter()
            .filter(|message| message.kind == MessageKind::PlanWatcher)
            .count(),
        1
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(model.requests().await.len(), 2);
    assert!(!snapshot.transcript.iter().any(|event| matches!(
        &event.payload,
        SessionEventPayload::ToolStarted { call, .. } if call.tool_name == "plan"
    )));

    agent.prompt(EndpointId::new(), "continue").await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
    let requests = model.requests().await;
    assert_eq!(requests.len(), 3);
    let next_prompt = serde_json::to_string(&requests[2].messages).unwrap();
    assert!(next_prompt.contains("execution_plan"));
    assert!(next_prompt.contains("finish plan support"));
}

#[tokio::test]
async fn reloaded_plan_stays_idle_until_an_explicit_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let first_model = ScriptedModelGateway::new([
        plan_step("survive restart", "in_progress"),
        ScriptedStep::text("saved"),
    ]);
    let first = AgentService::new(
        Arc::new(FsSessionRepository::new(&sessions).await.unwrap()),
        first_model,
        PolicyConfig::default(),
    );
    let agent = first
        .create(new_session(&workspace, SessionMode::FullAccess))
        .await
        .unwrap();
    let id = agent.id().clone();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "start").await.unwrap();
    wait_for_turn_end(&mut events).await;
    first.shutdown().await;

    let second_model = ScriptedModelGateway::new([ScriptedStep::text("resumed")]);
    let second = AgentService::new(
        Arc::new(FsSessionRepository::new(&sessions).await.unwrap()),
        second_model.clone(),
        PolicyConfig::default(),
    );
    let loaded = second.load(&id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(second_model.requests().await.is_empty());
    let snapshot = loaded.snapshot().await.unwrap();
    assert_eq!(snapshot.phase, RuntimePhase::Idle);
    assert!(snapshot.record.current_plan.is_some());

    let mut events = loaded.attach(EndpointId::new()).await.unwrap().events;
    loaded.prompt(EndpointId::new(), "continue").await.unwrap();
    wait_for_turn_end(&mut events).await;
    let requests = second_model.requests().await;
    assert_eq!(requests.len(), 1);
    let resumed = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(resumed.contains("survive restart"));
}

#[tokio::test]
async fn completed_ephemeral_session_is_sealed_until_kept() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::text("first answer"),
        ScriptedStep::text("continued answer"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service.create(ephemeral_session(dir.path())).await.unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;

    agent.prompt(EndpointId::new(), "first").await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
    let snapshot = agent.snapshot().await.unwrap();
    assert!(snapshot.record.info.ephemeral);
    assert!(snapshot.record.info.completed);
    assert!(snapshot.record.info.delete_after_ms.is_some());

    let error = agent
        .prompt(EndpointId::new(), "too late")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("has completed"));

    assert!(service.keep(agent.id()).await.unwrap());
    let kept = agent.snapshot().await.unwrap();
    assert!(!kept.record.info.ephemeral);
    assert!(kept.record.info.delete_after_ms.is_none());
    agent.prompt(EndpointId::new(), "continue").await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
}

#[tokio::test]
async fn failed_ephemeral_session_can_be_prompted_during_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::streamed_failure(Vec::new(), "boom"),
        ScriptedStep::text("recovered"),
    ]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service.create(ephemeral_session(dir.path())).await.unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;

    agent.prompt(EndpointId::new(), "fail first").await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnFailed { .. }
    ));
    let failed = agent.snapshot().await.unwrap();
    assert!(!failed.record.info.completed);
    assert!(failed.record.info.delete_after_ms.is_some());

    agent.prompt(EndpointId::new(), "retry").await.unwrap();
    assert!(
        agent
            .snapshot()
            .await
            .unwrap()
            .record
            .info
            .delete_after_ms
            .is_none()
    );
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
}

#[tokio::test]
async fn keep_wins_over_expired_ephemeral_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::streamed_failure(Vec::new(), "boom")]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let agent = service.create(ephemeral_session(dir.path())).await.unwrap();
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "fail").await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnFailed { .. }
    ));
    service.keep(agent.id()).await.unwrap();

    assert!(
        service
            .delete_if_ephemeral_expired(agent.id(), u64::MAX)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !service
            .status(agent.id())
            .await
            .unwrap()
            .record
            .info
            .ephemeral
    );
}

#[tokio::test]
async fn recovery_assigns_a_deadline_to_interrupted_ephemeral_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let repository = Arc::new(MemorySessionRepository::default());
    let service = AgentService::new(
        repository.clone(),
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );
    let agent = service.create(ephemeral_session(dir.path())).await.unwrap();
    let id = agent.id().clone();
    service.shutdown().await;
    let recovered = AgentService::new(
        repository,
        ScriptedModelGateway::new([]),
        PolicyConfig::default(),
    );

    let schedule = recovered
        .recover_ephemeral_sessions(100, 300)
        .await
        .unwrap();

    assert_eq!(schedule, [(id.clone(), 400)]);
    assert_eq!(
        recovered
            .status(&id)
            .await
            .unwrap()
            .record
            .info
            .delete_after_ms,
        Some(400)
    );
}

fn streamed_hosted_call(status: &str) -> ModelStreamEvent {
    ModelStreamEvent::ToolCall(StreamToolCall {
        tool_call_id: "fs-1".to_string(),
        tool_name: "file_search".to_string(),
        raw_input: json!({
            "type": "file_search_call",
            "id": "fs-1",
            "status": status
        }),
        status: status.to_string(),
    })
}

fn write(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn response_item<'a>(message: &'a ContextMessage, kind: &str) -> Option<&'a Value> {
    message
        .response_item_value()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some(kind))
}

fn response_output_text(message: &ContextMessage) -> Option<&str> {
    response_item(message, "message")?
        .get("content")?
        .as_array()?
        .iter()
        .find(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))?
        .get("text")?
        .as_str()
}

fn response_function_arguments(message: &ContextMessage) -> Option<Value> {
    let arguments = response_item(message, "function_call")?.get("arguments")?;
    match arguments {
        Value::String(encoded) => serde_json::from_str(encoded).ok(),
        value => Some(value.clone()),
    }
}

#[tokio::test]
async fn fork_copies_an_idle_session_without_changing_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::text("source answer")]);
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        model,
        PolicyConfig::default(),
    );
    let mut source_config = new_session(dir.path(), SessionMode::Confirm);
    source_config.parent_session_id = Some(SessionId::parse("session-parent").unwrap());
    let source = service.create(source_config).await.unwrap();
    let mut events = source.attach(EndpointId::new()).await.unwrap().events;

    source
        .prompt(EndpointId::new(), "original question")
        .await
        .unwrap();
    wait_for_turn_end(&mut events).await;
    let before = source.snapshot().await.unwrap();

    let fork = service.fork(source.id(), None).await.unwrap();
    let forked = fork.snapshot().await.unwrap();
    let after = source.snapshot().await.unwrap();

    assert_ne!(forked.record.info.id, before.record.info.id);
    assert_eq!(
        forked.record.info.parent_session_id,
        before.record.info.parent_session_id
    );
    assert_eq!(forked.record.info.title, before.record.info.title);
    assert_eq!(forked.record.info.cwd, before.record.info.cwd);
    assert_eq!(forked.record.config(), before.record.config());
    assert_eq!(forked.record.context, before.record.context);
    assert_eq!(
        serde_json::to_value(&forked.transcript).unwrap(),
        serde_json::to_value(&before.transcript).unwrap()
    );
    assert_eq!(after.record.info.id, before.record.info.id);
    assert_eq!(after.record.context, before.record.context);
    assert_eq!(
        serde_json::to_value(after.transcript).unwrap(),
        serde_json::to_value(before.transcript).unwrap()
    );
}

#[tokio::test]
async fn fork_rejects_a_running_source() {
    let dir = tempfile::tempdir().unwrap();
    let service = AgentService::new(
        Arc::new(MemorySessionRepository::default()),
        ScriptedModelGateway::new([ScriptedStep::delayed_text("late", 5_000)]),
        PolicyConfig::default(),
    );
    let source = service
        .create(new_session(dir.path(), SessionMode::Confirm))
        .await
        .unwrap();
    let accepted = source
        .prompt(EndpointId::new(), "original question")
        .await
        .unwrap();

    let error = service.fork(source.id(), None).await.err().unwrap();

    assert!(matches!(error, AgentServiceError::SessionBusy(id) if id == source.id().clone()));
    source.cancel(Some(accepted.turn_id)).await.unwrap();
    service.shutdown().await;
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
            automation_job: None,
            title: None,
            cwd: dir.path().to_path_buf(),
            rule_sources: Vec::new(),
            mode: SessionMode::FullAccess,
            llm: SessionLlmSettings::default(),
            ephemeral: false,
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
    let (first_load, second_load) = tokio::join!(restarted.load(&id), restarted.load(&id));
    let loaded = first_load.unwrap();
    assert!(Arc::ptr_eq(&loaded, &second_load.unwrap()));
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
    assert_eq!(snapshot.record.context.messages.len(), 6);
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
    assert_eq!(requests[1].messages.len(), 5);
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
async fn streamed_hosted_tool_is_created_once_and_updated_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let item = json!({
        "type": "file_search_call",
        "id": "fs-1",
        "status": "completed",
        "results": []
    });
    let reply = ModelReply {
        content: String::new(),
        reasoning: None,
        tool_calls: Vec::new(),
        remote_tool_calls: vec![json!({
            "id": "fs-1",
            "name": "file_search",
            "arguments": item,
            "status": "completed",
            "remote": true
        })],
        output_items: vec![item],
        finish_reason: FinishReason::Stop,
        usage: ModelUsage::default(),
    };
    let model = ScriptedModelGateway::new([ScriptedStep::streamed(
        vec![
            streamed_hosted_call("in_progress"),
            streamed_hosted_call("completed"),
        ],
        reply,
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
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "search").await.unwrap();

    let mut started = Vec::new();
    let mut updated = Vec::new();
    let mut completed = Vec::new();
    loop {
        match events.recv().await.unwrap().payload {
            SessionEventPayload::ToolStarted { call, .. } => started.push(call),
            SessionEventPayload::ToolUpdated { call, .. } => updated.push(call),
            SessionEventPayload::ToolCompleted { result, .. } => completed.push(result),
            SessionEventPayload::TurnCompleted { .. } => break,
            SessionEventPayload::TurnFailed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }

    assert_eq!(started.len(), 1);
    assert_eq!(started[0].tool_call_id, "fs-1");
    assert_eq!(started[0].status, "in_progress");
    assert!(!updated.is_empty());
    assert!(updated.iter().all(|call| call.tool_call_id == "fs-1"));
    assert_eq!(updated.last().unwrap().status, "completed");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].tool_call_id, "fs-1");
    assert!(
        agent
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .active_tool_calls
            .is_empty()
    );
}

#[tokio::test]
async fn failed_model_stream_completes_observed_tools() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::streamed_failure(
        vec![streamed_hosted_call("in_progress")],
        "stream failed",
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
    let mut events = agent.attach(EndpointId::new()).await.unwrap().events;
    agent.prompt(EndpointId::new(), "search").await.unwrap();

    let mut saw_start = false;
    let mut failed = None;
    loop {
        match events.recv().await.unwrap().payload {
            SessionEventPayload::ToolStarted { call, .. } => {
                saw_start = call.tool_call_id == "fs-1";
            }
            SessionEventPayload::ToolUpdated { call, .. } if call.status == "failed" => {
                failed = Some(call)
            }
            SessionEventPayload::TurnFailed { .. } => break,
            _ => {}
        }
    }
    assert!(saw_start);
    assert_eq!(
        failed.expect("streamed tool was not failed").tool_call_id,
        "fs-1"
    );
}

#[tokio::test]
async fn accepted_prompt_is_broadcast_to_origin_and_other_observers() {
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

    let accepted = agent
        .prompt(origin.clone(), "hello observers")
        .await
        .unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::UserPromptSubmitted {
                turn_id,
                origin,
                content,
                ..
            } = observer_events.recv().await.unwrap().payload
            {
                break (turn_id, origin, content);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(observed.0, accepted.turn_id);
    assert_eq!(observed.1, origin);
    assert_eq!(observed.2, "hello observers");

    let observed_origin = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let SessionEventPayload::UserPromptSubmitted {
                message_id,
                content,
                ..
            } = origin_events.recv().await.unwrap().payload
            {
                break (message_id, content);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(observed_origin.0, accepted.message_id);
    assert_eq!(observed_origin.1, "hello observers");
}

#[tokio::test]
async fn prompt_is_accepted_before_the_current_step_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::delayed_text("first answer", 500),
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
    let first = agent
        .prompt(endpoint.clone(), "first")
        .await
        .unwrap()
        .turn_id;

    tokio::time::timeout(Duration::from_secs(2), async {
        while model.requests().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let second = tokio::time::timeout(Duration::from_millis(100), agent.prompt(endpoint, "second"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, second.turn_id);
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
        snapshot
            .record
            .context
            .messages
            .iter()
            .rev()
            .find_map(response_output_text),
        Some("replacement")
    );
}

#[tokio::test]
async fn prompt_idle_rejects_an_active_turn() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::delayed_text("first answer", 150),
        ScriptedStep::text("second answer"),
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
    let first = agent
        .prompt(EndpointId::new(), "first")
        .await
        .unwrap()
        .turn_id;
    while model.requests().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let error = agent
        .prompt_idle(EndpointId::new(), "second")
        .await
        .unwrap_err();
    assert!(matches!(error, AgentServiceError::SessionBusy(_)));
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { turn_id } if turn_id == first
    ));

    let second = agent
        .prompt_idle(EndpointId::new(), "second")
        .await
        .unwrap()
        .turn_id;
    assert_ne!(first, second);
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { turn_id } if turn_id == second
    ));
    assert_eq!(model.requests().await.len(), 2);
}

#[tokio::test]
async fn targeted_internal_message_continues_only_the_expected_turn() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::delayed_text("not final", 100),
        ScriptedStep::text("final answer"),
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
    let turn = agent
        .prompt(EndpointId::new(), "work")
        .await
        .unwrap()
        .turn_id;
    while model.requests().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    agent
        .append_internal(turn.clone(), "time is up; answer now")
        .await
        .unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCompleted { turn_id } if turn_id == turn
    ));
    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == "time is up; answer now")
    );

    let error = agent
        .append_internal(turn.clone(), "too late")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentServiceError::TurnNotActive(turn_id) if turn_id == turn
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(model.requests().await.len(), 2);
}

#[tokio::test]
async fn cancel_keeps_an_already_accepted_queued_prompt() {
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
    let first = agent
        .prompt(EndpointId::new(), "first")
        .await
        .unwrap()
        .turn_id;
    while model.requests().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queued = agent.prompt(EndpointId::new(), "queued").await.unwrap();
    agent.cancel(Some(first.clone())).await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCancelled { turn_id } if turn_id == first
    ));
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(snapshot.transcript.iter().any(|event| {
        matches!(
            &event.payload,
            SessionEventPayload::UserPromptSubmitted { message_id, content, .. }
                if message_id == &queued.message_id && content.as_text() == Some("queued")
        )
    }));
    assert!(
        snapshot
            .record
            .context
            .messages
            .iter()
            .any(|message| message.content == "queued")
    );
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
    let turn = agent
        .prompt(EndpointId::new(), "first")
        .await
        .unwrap()
        .turn_id;
    while model.requests().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let second_agent = agent.clone();
    let second =
        tokio::spawn(async move { second_agent.prompt(EndpointId::new(), "second").await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let third_agent = agent.clone();
    let third = tokio::spawn(async move { third_agent.prompt(EndpointId::new(), "third").await });
    assert_eq!(second.await.unwrap().unwrap().turn_id, turn);
    assert_eq!(third.await.unwrap().unwrap().turn_id, turn);
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
    let turn = agent
        .prompt(EndpointId::new(), "work")
        .await
        .unwrap()
        .turn_id;
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
    let calls = requests[1]
        .messages
        .iter()
        .filter_map(response_function_arguments)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0]["patch"]
            .as_str()
            .unwrap()
            .contains("*** Add File: a")
    );
    assert!(
        calls[1]["patch"]
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
async fn resume_continues_idle_context_and_is_silent_while_running() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::text("initial answer"),
        ScriptedStep::delayed_text("resumed answer", 5_000),
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
    wait_for_turn_end(&mut events).await;

    let resumed = agent.resume(EndpointId::new()).await.unwrap().unwrap();
    while model.requests().await.len() < 2 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(agent.resume(EndpointId::new()).await.unwrap().is_none());
    agent.cancel(Some(resumed.turn_id.clone())).await.unwrap();
    assert!(matches!(
        wait_for_turn_end(&mut events).await,
        SessionEventPayload::TurnCancelled { turn_id } if turn_id == resumed.turn_id
    ));

    let requests = model.requests().await;
    assert!(requests[1].messages.iter().any(|message| {
        message.kind == MessageKind::Runtime && message.content.contains("<resume>")
    }));
    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(snapshot.transcript.iter().any(|event| matches!(
        &event.payload,
        SessionEventPayload::UserPromptSubmitted { content, .. }
            if content.as_text() == Some("/resume")
    )));
    assert!(!snapshot.record.context.messages.iter().any(|message| {
        message.role == dwo_agent_service::MessageRole::User && message.content == "/resume"
    }));
}

#[tokio::test]
async fn manual_compaction_uses_the_command_turn_without_adding_it_to_model_context() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::with_compaction(
        [
            ScriptedStep::text("old answer"),
            ScriptedStep::text("recent answer"),
        ],
        [ScriptedSummaryStep {
            summary: "older work summary".to_string(),
            input_tokens: 100,
            output_tokens: 10,
        }],
        ModelLimits {
            context_window_tokens: 200_000,
            max_output_tokens: 20_000,
            max_input_tokens: 180_000,
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
    agent
        .prompt(
            EndpointId::new(),
            format!("old work {}", "x".repeat(100_000)),
        )
        .await
        .unwrap();
    wait_for_turn_end(&mut events).await;
    agent
        .prompt(EndpointId::new(), "recent work")
        .await
        .unwrap();
    wait_for_turn_end(&mut events).await;
    let before = agent.snapshot().await.unwrap().usage.used;

    let compact = agent.compact(EndpointId::new()).await.unwrap();
    let mut response = None;
    let mut started_compaction = None;
    let mut completed_compaction = None;
    loop {
        match events.recv().await.unwrap().payload {
            SessionEventPayload::Notification {
                turn_id: Some(turn_id),
                category,
                data,
                ..
            } if turn_id == compact.turn_id && category == "compaction_started" => {
                assert_eq!(data["trigger"], "manual");
                started_compaction = data["compactionId"].as_str().map(str::to_string);
            }
            SessionEventPayload::Notification {
                turn_id: Some(turn_id),
                category,
                data,
                ..
            } if turn_id == compact.turn_id && category == "compaction_completed" => {
                completed_compaction = Some((
                    data["compactionId"].as_str().unwrap().to_string(),
                    data["summary"].as_str().map(str::to_string),
                ));
            }
            SessionEventPayload::AssistantCompleted {
                turn_id, content, ..
            } if turn_id == compact.turn_id => response = Some(content),
            SessionEventPayload::TurnCompleted { turn_id } if turn_id == compact.turn_id => break,
            SessionEventPayload::TurnFailed { turn_id, error } if turn_id == compact.turn_id => {
                panic!("manual compaction failed: {error}")
            }
            _ => {}
        }
    }

    let snapshot = agent.snapshot().await.unwrap();
    assert!(snapshot.usage.used < before);
    assert_eq!(snapshot.record.context.compaction.count, 1);
    let started_compaction = started_compaction.expect("compaction should start");
    let (completed_id, summary) = completed_compaction.expect("compaction should complete");
    assert_eq!(completed_id, started_compaction);
    assert_eq!(summary.as_deref(), Some("older work summary"));
    assert!(
        response
            .as_deref()
            .is_some_and(|content| content.starts_with("Context compacted from "))
    );
    assert!(snapshot.transcript.iter().any(|event| matches!(
        &event.payload,
        SessionEventPayload::UserPromptSubmitted { content, .. }
            if content.as_text() == Some("/compact")
    )));
    assert!(!snapshot.record.context.messages.iter().any(|message| {
        message.role == dwo_agent_service::MessageRole::User && message.content == "/compact"
    }));
    assert_eq!(model.requests().await.len(), 2);
    assert_eq!(model.summary_request_count().await, 1);
}

#[tokio::test]
async fn handoff_reports_a_completed_compaction_with_the_retained_summary() {
    let dir = tempfile::tempdir().unwrap();
    let handoff_summary = "Goal: finish the task. Done: inspected files. Next: implement.";
    let model = ScriptedModelGateway::new([
        ScriptedStep::tools(
            vec!["preparing handoff".to_string()],
            vec![json!({
                "id": "handoff-1",
                "name": "handoff",
                "arguments": {"handoff_text": handoff_summary}
            })],
        ),
        ScriptedStep::text("continued after handoff"),
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
    agent
        .prompt(EndpointId::new(), "continue cleanly")
        .await
        .unwrap();
    wait_for_turn_end(&mut events).await;

    let snapshot = agent.snapshot().await.unwrap();
    let started = snapshot
        .transcript
        .iter()
        .find_map(|event| match &event.payload {
            SessionEventPayload::Notification { category, data, .. }
                if category == "compaction_started" && data["trigger"] == "handoff" =>
            {
                data["compactionId"].as_str()
            }
            _ => None,
        });
    let completed = snapshot
        .transcript
        .iter()
        .find_map(|event| match &event.payload {
            SessionEventPayload::Notification { category, data, .. }
                if category == "compaction_completed" =>
            {
                Some((data["compactionId"].as_str()?, data["summary"].as_str()))
            }
            _ => None,
        });
    let started = started.expect("handoff compaction should start");
    let (completed_id, summary) = completed.expect("handoff compaction should complete");
    assert_eq!(completed_id, started);
    assert_eq!(summary, Some(handoff_summary));
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
    assert!(snapshot.transcript.iter().any(|event| matches!(
        event.payload,
        SessionEventPayload::Notification { ref category, ref data, .. }
            if category == "compaction_started" && data["trigger"] == "automatic"
    )));
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
    let call = requests[1]
        .messages
        .iter()
        .find_map(response_function_arguments)
        .unwrap();
    assert!(
        call["patch"]
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
    let compacted_call = requests[6]
        .messages
        .iter()
        .find_map(|message| response_item(message, "function_call"))
        .unwrap();
    assert_eq!(compacted_call["call_id"], "edit-recovery");
    let compacted_arguments = compacted_call["arguments"]
        .as_str()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .unwrap();
    assert!(
        compacted_arguments["patch"]
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
async fn switching_to_text_model_permanently_removes_images_from_model_context() {
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
            ScriptedStep::text("vision answer again"),
        ],
        [],
        [
            ("scripted-test-model".to_string(), unlimited),
            (
                "text-model".to_string(),
                ModelLimits {
                    context_window_tokens: 5_000,
                    max_output_tokens: 1_000,
                    max_input_tokens: 4_000,
                    compact_trigger_tokens: u64::MAX,
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

    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(snapshot.record.llm.model, "text-model");
    assert!(
        snapshot
            .record
            .context
            .messages
            .iter()
            .all(|message| !message.content.contains_images())
    );
    assert_eq!(snapshot.record.context.compaction.count, 0);
    assert_eq!(snapshot.record.context.compaction.summary, None);
    assert!(matches!(
        snapshot.transcript.iter().find_map(|event| match &event.payload {
            SessionEventPayload::UserPromptSubmitted { content, .. }
                if content.contains_images() => Some(content),
            _ => None,
        }),
        Some(content) if content == &image_prompt
    ));
    assert_eq!(model.summary_request_count().await, 0);

    agent.prompt(EndpointId::new(), "continue").await.unwrap();
    wait_for_turn_end(&mut subscription.events).await;
    let mut requests = model.requests().await;
    assert_eq!(requests[1].selection.model, "text-model");
    assert!(
        requests[1]
            .messages
            .iter()
            .all(|message| !message.content.contains_images())
    );

    agent
        .set_config(SessionConfigUpdate::Model(
            "scripted-test-model".to_string(),
        ))
        .await
        .unwrap();
    agent
        .prompt(EndpointId::new(), "continue again")
        .await
        .unwrap();
    wait_for_turn_end(&mut subscription.events).await;
    requests = model.requests().await;
    assert_eq!(requests[2].selection.model, "scripted-test-model");
    assert!(
        requests[2]
            .messages
            .iter()
            .all(|message| !message.content.contains_images())
    );
}

#[tokio::test]
async fn switching_providers_trims_private_response_items_but_keeps_visible_messages() {
    let dir = tempfile::tempdir().unwrap();
    let unlimited = ModelLimits {
        context_window_tokens: u64::MAX,
        max_output_tokens: u32::MAX,
        max_input_tokens: u64::MAX,
        compact_trigger_tokens: u64::MAX,
    };
    let model = ScriptedModelGateway::with_model_limits(
        [ScriptedStep::reasoning_text(
            "private summary",
            "visible answer",
        )],
        [],
        [
            ("scripted-test-model".to_string(), unlimited),
            ("next-model".to_string(), unlimited),
        ],
    )
    .with_providers([
        ("scripted-test-model".to_string(), "provider-a".to_string()),
        ("next-model".to_string(), "provider-b".to_string()),
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
    agent.prompt(EndpointId::new(), "remember").await.unwrap();
    wait_for_turn_end(&mut events).await;

    let before = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(before.record.context.messages.iter().any(|message| {
        message.provider.as_deref() == Some("provider-a")
            && message
                .response_item_value()
                .and_then(|item| item.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("reasoning")
    }));

    agent
        .set_config(SessionConfigUpdate::Model("next-model".to_string()))
        .await
        .unwrap();
    let after = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(after.record.context.provider.as_deref(), Some("provider-b"));
    assert!(after.record.context.messages.iter().all(|message| {
        message
            .response_item_value()
            .and_then(|item| item.get("type"))
            .and_then(serde_json::Value::as_str)
            != Some("reasoning")
    }));
    assert!(after.record.context.messages.iter().any(|message| {
        message
            .response_item_value()
            .and_then(|item| item.get("type"))
            .and_then(serde_json::Value::as_str)
            == Some("message")
    }));
    assert!(after.transcript.iter().any(|event| matches!(
        &event.payload,
        SessionEventPayload::AssistantCompleted { reasoning: Some(reasoning), .. }
            if reasoning == "private summary"
    )));
}

#[tokio::test]
async fn in_flight_old_provider_response_cannot_restore_stale_context_provider() {
    let dir = tempfile::tempdir().unwrap();
    let unlimited = ModelLimits {
        context_window_tokens: u64::MAX,
        max_output_tokens: u32::MAX,
        max_input_tokens: u64::MAX,
        compact_trigger_tokens: u64::MAX,
    };
    let model = ScriptedModelGateway::with_model_limits(
        [ScriptedStep::delayed_text("old provider answer", 100)],
        [],
        [
            ("scripted-test-model".to_string(), unlimited),
            ("next-model".to_string(), unlimited),
        ],
    )
    .with_providers([
        ("scripted-test-model".to_string(), "provider-a".to_string()),
        ("next-model".to_string(), "provider-b".to_string()),
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
    agent.prompt(EndpointId::new(), "start").await.unwrap();
    agent
        .set_config(SessionConfigUpdate::Model("next-model".to_string()))
        .await
        .unwrap();
    wait_for_turn_end(&mut events).await;

    let snapshot = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(snapshot.record.llm.model, "next-model");
    assert_eq!(
        snapshot.record.context.provider.as_deref(),
        Some("provider-b")
    );
    assert_eq!(
        snapshot
            .record
            .context
            .messages
            .iter()
            .rev()
            .find_map(response_output_text),
        Some("old provider answer")
    );
}

#[tokio::test]
async fn text_model_transition_removes_images_without_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let limits = ModelLimits {
        context_window_tokens: 1_000,
        max_output_tokens: 100,
        max_input_tokens: 900,
        compact_trigger_tokens: 800,
    };
    let model = ScriptedModelGateway::with_model_limits_and_capabilities(
        [
            ScriptedStep::text("vision answer"),
            ScriptedStep::text("text answer"),
        ],
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
        model.clone(),
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
    agent
        .set_config(SessionConfigUpdate::Model("text-model".to_string()))
        .await
        .unwrap();
    let before = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert!(
        before
            .record
            .context
            .messages
            .iter()
            .all(|message| !message.content.contains_images())
    );

    agent.prompt(EndpointId::new(), "continue").await.unwrap();
    wait_for_turn_end(&mut subscription.events).await;
    let after = agent.attach(EndpointId::new()).await.unwrap().snapshot;
    assert_eq!(after.record.llm.model, "text-model");
    assert_eq!(after.record.context.compaction.count, 0);
    assert_eq!(after.record.context.compaction.summary, None);
    assert!(
        after
            .record
            .context
            .messages
            .iter()
            .all(|message| !message.content.contains_images())
    );
    assert_eq!(model.summary_request_count().await, 0);
}

#[tokio::test]
async fn model_switch_is_allowed_while_an_image_turn_is_active() {
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
        .unwrap()
        .turn_id;

    agent
        .set_config(SessionConfigUpdate::Model("text-model".to_string()))
        .await
        .unwrap();
    assert_eq!(
        agent
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record
            .llm
            .model,
        "text-model"
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
            automation_job: None,
            title: Some("profile test".to_string()),
            cwd: cwd.clone(),
            rule_sources: Vec::new(),
            mode: SessionMode::FullAccess,
            llm: SessionLlmSettings::default(),
            ephemeral: false,
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
    let step = attached.snapshot.active_step.as_ref().unwrap();
    assert_eq!(step.step_id, 1);
    assert_eq!(step.revision, 1);
    assert_eq!(step.response, "hel");
    assert!(attached.snapshot.seq >= first_delta_seq);

    let next = attached.events.recv().await.unwrap();
    assert!(next.seq > attached.snapshot.seq);
    assert!(matches!(
        wait_for_turn_end(&mut attached.events).await,
        SessionEventPayload::TurnCompleted { .. }
    ));
}

#[tokio::test]
async fn slow_observer_receives_a_full_step_snapshot_before_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::Response {
        chunks: vec!["x".to_string(); 300],
        tool_calls: Vec::new(),
        finish_reason: dwo_agent_service::FinishReason::Stop,
        delay_ms: 0,
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
    let mut subscription = agent.attach(EndpointId::new()).await.unwrap();
    agent.prompt(EndpointId::new(), "stream").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut recovered = None;
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = subscription.events.recv().await {
            match event.payload {
                SessionEventPayload::StepSnapshot { step } => recovered = Some(step),
                SessionEventPayload::TurnCompleted { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let recovered = recovered.expect("slow observer must receive a step snapshot");
    assert_eq!(recovered.step_id, 1);
    assert_eq!(recovered.revision, 300);
    assert_eq!(recovered.response.len(), 300);
}

#[tokio::test]
async fn reconnect_replays_only_checkpoints_after_the_requested_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([ScriptedStep::text("answer")]);
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
    agent.prompt(EndpointId::new(), "question").await.unwrap();
    wait_for_turn_end(&mut events).await;

    let replay = agent
        .attach_from(EndpointId::new(), Some(1))
        .await
        .unwrap()
        .snapshot;
    assert_eq!(replay.checkpoint_cursor, 2);
    assert_eq!(replay.transcript.len(), 1);
    assert!(matches!(
        replay.transcript[0].payload,
        SessionEventPayload::AssistantCompleted { .. }
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
    let turn_id = agent
        .prompt(EndpointId::new(), "wait")
        .await
        .unwrap()
        .turn_id;
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
    let tool_output = &snapshot
        .record
        .context
        .messages
        .iter()
        .find(|message| message.role == dwo_agent_service::MessageRole::Tool)
        .unwrap()
        .content;
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
        snapshot
            .record
            .context
            .messages
            .iter()
            .rev()
            .find_map(response_output_text),
        Some("continued")
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
    assert_eq!(snapshot.record.context.messages.len(), 7);
    assert_eq!(
        snapshot
            .record
            .context
            .messages
            .iter()
            .rev()
            .find_map(response_output_text),
        Some("persisted")
    );
    let transcript_kinds = snapshot
        .transcript
        .iter()
        .map(|event| match &event.payload {
            SessionEventPayload::UserPromptSubmitted { .. } => "user",
            SessionEventPayload::AssistantDelta { .. } => "assistant_delta",
            SessionEventPayload::AssistantReasoningDelta { .. } => "reasoning_delta",
            SessionEventPayload::AssistantCompleted { .. } => "assistant_completed",
            SessionEventPayload::ToolStarted { .. } => "tool_started",
            SessionEventPayload::FileChanged { .. } => "file_changed",
            SessionEventPayload::ToolCompleted { .. } => "tool_completed",
            _ => "other",
        })
        .collect::<Vec<_>>();
    assert_eq!(
        transcript_kinds,
        [
            "user",
            "assistant_completed",
            "tool_started",
            "file_changed",
            "tool_completed",
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
async fn model_change_remembers_reasoning_per_model() {
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
    assert_eq!(
        agent
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record
            .llm
            .reasoning
            .as_deref(),
        Some("high")
    );
    service
        .set_config(&id, SessionConfigUpdate::Reasoning(Some("low".to_string())))
        .await
        .unwrap();
    service
        .set_config(
            &id,
            SessionConfigUpdate::Model("scripted-test-model".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        agent
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record
            .llm
            .reasoning
            .as_deref(),
        Some("high")
    );
    service
        .set_config(&id, SessionConfigUpdate::Model("next-model".to_string()))
        .await
        .unwrap();
    assert_eq!(
        agent
            .attach(EndpointId::new())
            .await
            .unwrap()
            .snapshot
            .record
            .llm
            .reasoning
            .as_deref(),
        Some("low")
    );
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
    assert_eq!(config.reasoning.as_deref(), Some("low"));
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

async fn wait_for_turn_end_after_retry(
    events: &mut tokio::sync::mpsc::Receiver<dwo_agent_service::SessionEvent>,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.expect("event stream closed");
            assert!(!matches!(
                event.payload,
                SessionEventPayload::TurnFailed { .. }
            ));
            if matches!(event.payload, SessionEventPayload::TurnCompleted { .. }) {
                break;
            }
        }
    })
    .await
    .expect("turn did not finish")
}

#[tokio::test(start_paused = true)]
async fn interrupted_stream_retries_inside_the_same_turn() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::streamed_interrupt(vec![ModelStreamEvent::TextDelta("partial ".to_string())]),
        ScriptedStep::text("final answer"),
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
    let accepted = agent.prompt(origin.clone(), "hello").await.unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await.unwrap().payload,
                SessionEventPayload::Notification { ref category, .. }
                    if category == "model_retrying"
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let retrying = agent.snapshot().await.unwrap();
    assert_eq!(retrying.phase, RuntimePhase::Running);
    assert_eq!(retrying.active_turn_id.as_ref(), Some(&accepted.turn_id));
    agent
        .prompt(origin, "include this before retry")
        .await
        .unwrap();
    wait_for_turn_end_after_retry(&mut events).await;

    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    let retry_context = serde_json::to_string(&requests[1].messages).unwrap();
    assert!(retry_context.contains("partial "));
    assert!(retry_context.contains("include this before retry"));
    let snapshot = agent.snapshot().await.unwrap();
    assert_eq!(snapshot.phase, RuntimePhase::Idle);
    assert!(snapshot.transcript.iter().any(|event| matches!(
        &event.payload,
        SessionEventPayload::AssistantInterrupted { turn_id, content, .. }
            if turn_id == &accepted.turn_id && content == "partial "
    )));
    assert!(snapshot.transcript.iter().any(|event| matches!(
        &event.payload,
        SessionEventPayload::Notification { turn_id: Some(turn_id), category, .. }
            if turn_id == &accepted.turn_id && category == "model_retrying"
    )));
}

#[tokio::test(start_paused = true)]
async fn repeated_interruptions_append_partial_assistant_messages_without_resume_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelGateway::new([
        ScriptedStep::streamed_interrupt(vec![ModelStreamEvent::TextDelta("partial ".to_string())]),
        ScriptedStep::streamed_interrupt(vec![ModelStreamEvent::TextDelta("more ".to_string())]),
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
    agent.prompt(origin.clone(), "hello").await.unwrap();

    wait_for_turn_end_after_retry(&mut events).await;

    let requests = model.requests().await;
    assert_eq!(requests.len(), 3);
    let partials = requests
        .iter()
        .map(|request| {
            request
                .messages
                .iter()
                .filter(|message| message.role == dwo_agent_service::MessageRole::Assistant)
                .filter_map(|message| message.content.as_text())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert!(partials[0].is_empty());
    assert_eq!(partials[1], vec!["partial "]);
    assert_eq!(partials[2], vec!["partial ", "more "]);
    assert!(
        requests
            .iter()
            .all(|request| request.messages.iter().all(|message| {
                message
                    .content
                    .as_text()
                    .is_none_or(|text| !text.contains("<resume>"))
            }))
    );
}
