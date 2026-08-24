use std::time::Duration;

use dwo_context::{CompactionView, ContentBlock, ContextMessage, MessageContent};
use dwo_model_client::{
    AgentModelConfig, ConfiguredModelClient, FinishReason, ModelCatalog, ModelClient,
    ModelClientConfig, ModelClientError, ModelSelection, ModelStreamEvent,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

async fn one_response_server(response: String) -> (String, oneshot::Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, request_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        request_tx.send(request).ok();
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    (format!("http://{address}"), request_rx)
}

async fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "connection closed before HTTP request completed");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(header_end) = find_bytes(&bytes, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or_default();
            break (header_end + 4, content_length);
        }
    };
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn catalog() -> &'static str {
    r#"
families:
  test:
    models:
      test-model:
        contextWindowTokens: 100000
        maxOutputTokens: 4096
        capabilities:
          imageInput: true
          toolCalls: true
        hostedTools:
          webSearch:
            type: web_search
        reasoning:
          Low:
            reasoning:
              effort: low
            extra_body:
              thinking:
                type: enabled
"#
}

fn agent(endpoint: &str) -> String {
    format!(
        r#"
default:
  model: local/test-model
  reasoning: Low
compactionTriggerRatio: 0.8
providers:
  local:
    baseUrl: {endpoint}
    request:
      requestTimeoutMs: 5000
      streamIdleTimeoutMs: 5000
    extraBody:
      extra_body:
        provider_flag: true
    models:
      Chat:
        modelId: test-model
        profile: test/test-model
"#
    )
}

#[tokio::test]
async fn streaming_turn_emits_deltas_and_assembles_tool_calls() {
    let chunks = [
        json!({"type":"response.reasoning_summary_text.delta","item_id":"reason-1","output_index":0,"summary_index":0,"delta":"think "}).to_string(),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"reason-1","output_index":0,"summary_index":0,"delta":"more"}).to_string(),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"reason-1","output_index":0,"summary_index":1,"delta":"next"}).to_string(),
        json!({"type":"response.output_text.delta","delta":"working"}).to_string(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"file_search_call","id":"fs-1","status":"in_progress"}}).to_string(),
        json!({"type":"response.file_search_call.searching","output_index":0,"item_id":"fs-1"}).to_string(),
        json!({"type":"response.file_search_call.completed","output_index":0,"item_id":"fs-1"}).to_string(),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"file_search_call","id":"fs-1","status":"completed","results":[]}}).to_string(),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call-1","name":"terminal","arguments":""}}).to_string(),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"action\":\"run\",\"comm"}).to_string(),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"and\":\"echo hi\"}"}).to_string(),
        json!({"type":"response.function_call_arguments.done","output_index":1,"arguments":"{\"action\":\"run\",\"command\":\"echo hi\"}"}).to_string(),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call-1","name":"terminal","arguments":"{\"action\":\"run\",\"command\":\"echo hi\"}"}}).to_string(),
        json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"think more"},{"type":"summary_text","text":"next"}]},{"type":"file_search_call","id":"fs-1","status":"completed","results":[]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]},{"type":"function_call","call_id":"call-1","name":"terminal","arguments":"{\"action\":\"run\",\"command\":\"echo hi\"}"}],"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}}}).to_string(),
    ];
    let sse = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
    );
    let (endpoint, request_rx) = one_response_server(response).await;
    let client = ConfiguredModelClient::from_yaml(catalog(), &agent(&endpoint)).unwrap();
    let limits = client.model_limits("local/test-model").unwrap();
    assert_eq!(limits.context_window_tokens, 100_000);
    assert_eq!(limits.max_output_tokens, 4_096);
    assert_eq!(limits.max_input_tokens, 95_904);
    assert_eq!(limits.compact_trigger_tokens, 76_723);
    assert_eq!(client.default_model(), "local/test-model");
    assert!(client.supports_image_input("local/test-model").unwrap());
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let messages = vec![
        ContextMessage::system("system prompt"),
        ContextMessage::user(MessageContent::blocks(vec![
            ContentBlock::text("hello"),
            ContentBlock::image("image/png", "aGVsbG8="),
        ])),
    ];
    let tools = vec![json!({"type":"function","function":{"name":"terminal"}})];
    let cancellation = CancellationToken::new();
    let reply = client
        .stream_turn(
            ModelSelection {
                model: "local/test-model".to_string(),
                reasoning: Some("Low".to_string()),
            },
            &messages,
            &tools,
            events_tx,
            &cancellation,
        )
        .await
        .unwrap();

    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::ReasoningDelta("think ".to_string())
    );
    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::ReasoningDelta("more".to_string())
    );
    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::ReasoningDelta("\n\n".to_string())
    );
    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::ReasoningDelta("next".to_string())
    );
    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::TextDelta("working".to_string())
    );
    let ModelStreamEvent::ToolCall(hosted) = events_rx.recv().await.unwrap() else {
        panic!("expected completed hosted tool call");
    };
    assert_eq!(hosted.tool_call_id, "fs-1");
    assert_eq!(hosted.tool_name, "file_search");
    assert_eq!(hosted.status, "completed");
    assert_eq!(hosted.raw_input["results"], json!([]));
    let ModelStreamEvent::ToolCall(started) = events_rx.recv().await.unwrap() else {
        panic!("expected streamed tool call start");
    };
    assert_eq!(started.tool_call_id, "call-1");
    assert_eq!(started.status, "pending");
    assert_eq!(started.raw_input["command"], "echo hi");
    assert!(events_rx.try_recv().is_err());
    assert_eq!(reply.content, "working");
    assert_eq!(reply.reasoning.as_deref(), Some("think more\n\nnext"));
    assert_eq!(reply.reasoning_content, None);
    assert_eq!(reply.finish_reason, FinishReason::ToolCalls);
    assert_eq!(reply.tool_calls[0]["id"], "call-1");
    assert_eq!(reply.tool_calls[0]["name"], "terminal");
    assert_eq!(reply.tool_calls[0]["arguments"]["command"], "echo hi");
    assert_eq!(reply.remote_tool_calls[0]["id"], "fs-1");
    assert_eq!(reply.usage.total_tokens, 14);

    let request = request_rx.await.unwrap();
    assert_eq!(request["stream"], true);
    assert_eq!(request["model"], "test-model");
    assert_eq!(request["max_output_tokens"], 4096);
    assert_eq!(request["input"][0]["role"], "system");
    assert_eq!(request["input"][1]["content"][0]["type"], "input_text");
    assert_eq!(
        request["input"][1]["content"][1]["image_url"],
        "data:image/png;base64,aGVsbG8="
    );
    assert_eq!(request["tools"].as_array().unwrap().len(), 2);
    assert_eq!(request["tools"][0]["type"], "web_search");
    assert_eq!(request["tools"][1]["name"], "terminal");
    assert_eq!(request["reasoning"]["effort"], "low");
    assert_eq!(request["extra_body"]["provider_flag"], true);
    assert_eq!(request["extra_body"]["thinking"]["type"], "enabled");
}

#[tokio::test]
async fn streaming_turn_preserves_plaintext_reasoning_content() {
    let chunks = [
        json!({"type":"response.reasoning_text.delta","item_id":"reason-1","output_index":0,"content_index":0,"delta":"inspect "}).to_string(),
        json!({"type":"response.reasoning_text.delta","item_id":"reason-1","output_index":0,"content_index":0,"delta":"files"}).to_string(),
        json!({"type":"response.output_text.delta","delta":"done"}).to_string(),
        json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}).to_string(),
    ];
    let sse = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
    );
    let (endpoint, _request_rx) = one_response_server(response).await;
    let client = ConfiguredModelClient::from_yaml(catalog(), &agent(&endpoint)).unwrap();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let messages = [
        ContextMessage::system("system prompt"),
        ContextMessage::user("inspect"),
    ];
    let reply = client
        .stream_turn(
            ModelSelection {
                model: "local/test-model".to_string(),
                reasoning: Some("Low".to_string()),
            },
            &messages,
            &[],
            events_tx,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::ReasoningDelta("inspect ".to_string())
    );
    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::ReasoningDelta("files".to_string())
    );
    assert_eq!(reply.reasoning, None);
    assert_eq!(reply.reasoning_content.as_deref(), Some("inspect files"));
    assert_eq!(
        reply.transcript_reasoning().as_deref(),
        Some("inspect files")
    );
}

#[tokio::test]
async fn streamed_function_call_waits_for_complete_output_item_without_arguments_done() {
    let chunks = [
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"terminal","arguments":""}}).to_string(),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"command\":\"echo hi\"}"}).to_string(),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"terminal","arguments":"{\"command\":\"echo hi\"}"}}).to_string(),
        json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","call_id":"call-1","name":"terminal","arguments":"{\"command\":\"echo hi\"}"}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}).to_string(),
    ];
    let sse = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
    );
    let (endpoint, _request_rx) = one_response_server(response).await;
    let client = ConfiguredModelClient::from_yaml(catalog(), &agent(&endpoint)).unwrap();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

    let reply = client
        .stream_turn(
            ModelSelection {
                model: "local/test-model".to_string(),
                reasoning: None,
            },
            &[
                ContextMessage::system("system prompt"),
                ContextMessage::user("run it"),
            ],
            &[json!({"type":"function","function":{"name":"terminal"}})],
            events_tx,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    let ModelStreamEvent::ToolCall(call) = events_rx.recv().await.unwrap() else {
        panic!("expected complete streamed tool call");
    };
    assert_eq!(call.tool_call_id, "call-1");
    assert_eq!(call.raw_input["command"], "echo hi");
    assert!(events_rx.try_recv().is_err());
    assert_eq!(reply.tool_calls[0]["arguments"]["command"], "echo hi");
}

#[tokio::test]
async fn summary_uses_non_streaming_request_without_tools() {
    let payload = json!({
        "status":"completed",
        "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"summary text"}]}],
        "usage":{"input_tokens":20,"output_tokens":3,"total_tokens":23}
    });
    let body = payload.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (endpoint, request_rx) = one_response_server(response).await;
    let client = ConfiguredModelClient::from_yaml(catalog(), &agent(&endpoint)).unwrap();
    let reply = client
        .summarize(
            ModelSelection {
                model: "local/test-model".to_string(),
                reasoning: None,
            },
            CompactionView {
                instruction: "compact instruction".to_string(),
                messages: vec![ContextMessage::user("history")],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply.summary, "summary text");
    assert_eq!(reply.usage.total_tokens, 23);

    let request = request_rx.await.unwrap();
    assert_eq!(request["stream"], false);
    assert!(request.get("tools").is_none());
    assert_eq!(
        request["input"][0]["content"][0]["text"],
        "compact instruction"
    );
    assert_eq!(request["input"][1]["content"][0]["text"], "history");
}

#[tokio::test]
async fn completion_uses_non_streaming_request_without_tools() {
    let payload = json!({
        "status":"completed",
        "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Short title"}]}],
        "usage":{"input_tokens":8,"output_tokens":2,"total_tokens":10}
    });
    let body = payload.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (endpoint, request_rx) = one_response_server(response).await;
    let client = ConfiguredModelClient::from_yaml(catalog(), &agent(&endpoint)).unwrap();
    let reply = client
        .complete(
            ModelSelection {
                model: "local/test-model".to_string(),
                reasoning: None,
            },
            vec![
                ContextMessage::system("Generate a title"),
                ContextMessage::user("Investigate flaky tests"),
            ],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply.content, "Short title");
    assert_eq!(reply.usage.total_tokens, 10);

    let request = request_rx.await.unwrap();
    assert_eq!(request["stream"], false);
    assert!(request.get("tools").is_none());
    assert_eq!(
        request["input"][0]["content"][0]["text"],
        "Generate a title"
    );
    assert_eq!(
        request["input"][1]["content"][0]["text"],
        "Investigate flaky tests"
    );
}

#[tokio::test]
async fn responses_always_uses_max_output_tokens() {
    let payload = json!({
        "status":"completed",
        "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]
    });
    let body = payload.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (endpoint, request_rx) = one_response_server(response).await;
    let client = ConfiguredModelClient::from_yaml(catalog(), &agent(&endpoint)).unwrap();
    client
        .complete(
            ModelSelection {
                model: "local/test-model".to_string(),
                reasoning: None,
            },
            vec![
                ContextMessage::system("system"),
                ContextMessage::user("hello"),
            ],
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let request = request_rx.await.unwrap();
    assert_eq!(request["max_output_tokens"], 4096);
    assert!(request.get("max_tokens").is_none());
    assert!(request.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn cancellation_interrupts_an_in_flight_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut stream).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let client =
        ConfiguredModelClient::from_yaml(catalog(), &agent(&format!("http://{address}"))).unwrap();
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let messages = vec![
        ContextMessage::system("system"),
        ContextMessage::user("hello"),
    ];
    let error = client
        .stream_turn(
            ModelSelection {
                model: "local/test-model".to_string(),
                reasoning: None,
            },
            &messages,
            &[],
            events,
            &cancellation,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ModelClientError::Cancelled));
}

#[test]
fn config_rejects_reserved_extra_body_overrides() {
    let invalid = r#"
families:
  local:
    models:
      test:
        contextWindowTokens: 100000
        maxOutputTokens: 4096
        extraBody:
          input: []
"#;
    let error = ModelCatalog::from_yaml(invalid).unwrap_err();
    assert!(error.to_string().contains("reserved field input"));
}

#[test]
fn official_provider_expands_the_complete_family() {
    let catalog = ModelCatalog::builtin().unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
default:
  model: deepseek/deepseek-v4-pro
  reasoning: High
compactionTriggerRatio: 0.5
providers:
  deepseek:
    apiKeyEnv: DEEPSEEK_API_KEY
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();

    assert_eq!(resolved.providers.len(), 1);
    assert_eq!(
        resolved.providers["deepseek"].base_url,
        "https://api.deepseek.com"
    );
    assert_eq!(resolved.default_model, "deepseek/deepseek-v4-pro");
    assert_eq!(resolved.default_reasoning.as_deref(), Some("High"));
    let model = &resolved.models["deepseek/deepseek-v4-pro"];
    assert_eq!(model.model_name, "deepseek-v4-pro");
    assert_eq!(model.max_input_tokens().unwrap(), 616_000);
    assert_eq!(model.compaction_trigger_ratio, 0.5);
}

#[test]
fn custom_provider_maps_display_names_and_multiple_families() {
    let catalog = ModelCatalog::builtin().unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
default:
  model: newapi/ds-v4-pro
providers:
  newapi:
    baseUrl: https://gateway.example.com/v1
    apiKeyEnv: NEW_API_KEY
    models:
      "5.6 Terra":
        modelId: gpt-5.6-terra
        profile: openai/gpt-5.6-terra
      "Grok 4.6":
        modelId: grok-4.6
        profile: grok/grok-4.6
      "DeepSeek V4 Pro":
        modelId: ds-v4-pro
        profile: deepseek/deepseek-v4-pro
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();

    assert_eq!(resolved.providers.len(), 1);
    assert_eq!(resolved.models.len(), 3);
    assert_eq!(
        resolved.models["newapi/ds-v4-pro"].model_name,
        "DeepSeek V4 Pro"
    );
    assert_eq!(resolved.models["newapi/ds-v4-pro"].model_id, "ds-v4-pro");
    assert_eq!(
        resolved.models["newapi/gpt-5.6-terra"].context_owner_id(),
        "newapi/openai"
    );
    assert_eq!(
        resolved.models["newapi/grok-4.6"].context_owner_id(),
        "newapi/grok"
    );

    let client = ConfiguredModelClient::new(&catalog, &agent).unwrap();
    assert_eq!(
        client.context_owner_id("newapi/gpt-5.6-terra").unwrap(),
        "newapi/openai"
    );
    assert_eq!(
        client.context_owner_id("newapi/grok-4.6").unwrap(),
        "newapi/grok"
    );
    assert_eq!(client.provider_id("newapi/grok-4.6").unwrap(), "newapi");
}

#[test]
fn explicit_official_models_form_an_allowlist_and_infer_profiles() {
    let catalog = ModelCatalog::builtin().unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
default:
  model: openai/gpt-5.6-terra
providers:
  openai:
    baseUrl: https://compatible.example.com/v1
    models:
      "5.6 Terra":
        modelId: gpt-5.6-terra
        hostedTools: []
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();

    assert_eq!(resolved.models.len(), 1);
    let model = &resolved.models["openai/gpt-5.6-terra"];
    assert_eq!(model.model_name, "5.6 Terra");
    assert!(model.hosted_tools.is_empty());
    assert_eq!(
        resolved.providers["openai"].base_url,
        "https://compatible.example.com/v1"
    );
}

#[test]
fn custom_provider_requires_base_url_models_and_profiles() {
    for source in [
        r#"
default:
  model: custom/test
providers:
  custom:
    models:
      Test:
        modelId: test
        profile: openai/gpt-5.6-terra
"#,
        r#"
default:
  model: custom/test
providers:
  custom:
    baseUrl: https://example.com/v1
"#,
        r#"
default:
  model: custom/test
providers:
  custom:
    baseUrl: https://example.com/v1
    models:
      Test:
        modelId: test
"#,
    ] {
        let agent = AgentModelConfig::from_yaml(source).unwrap();
        assert!(ModelClientConfig::resolve(&ModelCatalog::builtin().unwrap(), &agent).is_err());
    }
}

#[test]
fn duplicate_upstream_model_ids_are_rejected() {
    let agent = AgentModelConfig::from_yaml(
        r#"
default:
  model: custom/same
providers:
  custom:
    baseUrl: https://example.com/v1
    models:
      First:
        modelId: same
        profile: openai/gpt-5.6-terra
      Second:
        modelId: same
        profile: grok/grok-4.6
"#,
    )
    .unwrap();
    let error = ModelClientConfig::resolve(&ModelCatalog::builtin().unwrap(), &agent).unwrap_err();
    assert!(error.to_string().contains("duplicate modelId same"));
}

#[test]
fn custom_model_directory_adds_a_family() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("minimax.yaml"),
        r#"
models:
  minimax-m2.5:
    contextWindowTokens: 200000
    maxOutputTokens: 32000
    capabilities:
      toolCalls: true
    defaultReasoningMode: High
    reasoning:
      High:
        reasoning:
          effort: high
"#,
    )
    .unwrap();

    let mut catalog = ModelCatalog::builtin().unwrap();
    catalog.merge_model_directory(directory.path()).unwrap();
    assert!(catalog.families.contains_key("minimax"));

    let agent = AgentModelConfig::from_yaml(
        r#"
default:
  model: gateway/minimax-m2.5
providers:
  gateway:
    baseUrl: https://gateway.example.com/v1
    models:
      "MiniMax M2.5":
        modelId: minimax-m2.5
        profile: minimax/minimax-m2.5
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();
    assert_eq!(
        resolved.models["gateway/minimax-m2.5"].model_name,
        "MiniMax M2.5"
    );
}

#[tokio::test]
async fn interrupted_stream_reports_stream_interrupted_error() {
    let chunks = [
        json!({"type":"response.output_text.delta","delta":"hello "}).to_string(),
        json!({"type":"response.output_text.delta","delta":"world"}).to_string(),
    ];
    let sse = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
    );
    let (endpoint, _request_rx) = one_response_server(response).await;
    let client = ConfiguredModelClient::from_yaml(catalog(), &agent(&endpoint)).unwrap();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let messages = vec![
        ContextMessage::system("system"),
        ContextMessage::user("hello"),
    ];
    let cancellation = CancellationToken::new();
    let error = client
        .stream_turn(
            ModelSelection {
                model: "local/test-model".to_string(),
                reasoning: None,
            },
            &messages,
            &[],
            events_tx,
            &cancellation,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ModelClientError::StreamInterrupted {
            text_chars: 11,
            has_tool_calls: false
        }
    ));
    assert!(error.is_stream_interrupted());
    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::TextDelta("hello ".to_string())
    );
    assert_eq!(
        events_rx.recv().await.unwrap(),
        ModelStreamEvent::TextDelta("world".to_string())
    );
}
