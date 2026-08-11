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

fn catalog(endpoint: &str) -> String {
    format!(
        r#"
providers:
  local:
    endpoint: {endpoint}
    request:
      requestTimeoutMs: 5000
      streamIdleTimeoutMs: 5000
      maxRetries: 0
      retryBaseDelayMs: 1
    body:
      extra_body:
        provider_flag: true
    models:
      test-model:
        contextWindowTokens: 100000
        maxOutputTokens: 4096
        compactThreshold: 0.8
        capabilities:
          imageInput: true
          toolCalls: true
        hostedTools:
          - type: web_search
        reasoning:
          high:
            reasoning:
              effort: high
            extra_body:
              thinking:
                type: enabled
"#
    )
}

fn agent() -> &'static str {
    r#"
defaultModelName: chat
providers:
  local:
    type: local
models:
  - modelName: chat
    provider: local
    modelId: test-model
"#
}

#[tokio::test]
async fn streaming_turn_emits_deltas_and_assembles_tool_calls() {
    let chunks = [
        json!({"type":"response.reasoning_summary_text.delta","delta":"think "}).to_string(),
        json!({"type":"response.output_text.delta","delta":"working"}).to_string(),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"file_search_call","id":"fs-1","status":"in_progress"}}).to_string(),
        json!({"type":"response.file_search_call.searching","output_index":0,"item_id":"fs-1"}).to_string(),
        json!({"type":"response.file_search_call.completed","output_index":0,"item_id":"fs-1"}).to_string(),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"file_search_call","id":"fs-1","status":"completed","results":[]}}).to_string(),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call-1","name":"terminal","arguments":""}}).to_string(),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"action\":\"run\",\"comm"}).to_string(),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"and\":\"echo hi\"}"}).to_string(),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call-1","name":"terminal","arguments":"{\"action\":\"run\",\"command\":\"echo hi\"}"}}).to_string(),
        json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"think "}]},{"type":"file_search_call","id":"fs-1","status":"completed","results":[]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]},{"type":"function_call","call_id":"call-1","name":"terminal","arguments":"{\"action\":\"run\",\"command\":\"echo hi\"}"}],"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}}}).to_string(),
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
    let client = ConfiguredModelClient::from_yaml(&catalog(&endpoint), agent()).unwrap();
    let limits = client.model_limits("chat").unwrap();
    assert_eq!(limits.context_window_tokens, 100_000);
    assert_eq!(limits.max_output_tokens, 4_096);
    assert_eq!(limits.max_input_tokens, 95_904);
    assert_eq!(limits.compact_trigger_tokens, 76_723);
    assert_eq!(client.default_model_name(), "chat");
    assert!(client.supports_image_input("chat").unwrap());
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
                model: "chat".to_string(),
                reasoning: Some("high".to_string()),
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
        ModelStreamEvent::TextDelta("working".to_string())
    );
    for expected in ["in_progress", "in_progress", "completed", "completed"] {
        let ModelStreamEvent::ToolCall(call) = events_rx.recv().await.unwrap() else {
            panic!("expected hosted tool progress");
        };
        assert_eq!(call.tool_call_id, "fs-1");
        assert_eq!(call.tool_name, "file_search");
        assert_eq!(call.status, expected);
    }
    let ModelStreamEvent::ToolCall(started) = events_rx.recv().await.unwrap() else {
        panic!("expected streamed tool call start");
    };
    assert_eq!(started.tool_call_id, "call-1");
    assert_eq!(started.status, "pending");
    assert_eq!(started.raw_input, json!({}));
    let ModelStreamEvent::ToolCall(updated) = events_rx.recv().await.unwrap() else {
        panic!("expected streamed tool call input update");
    };
    assert_eq!(updated.tool_call_id, "call-1");
    assert_eq!(updated.raw_input["command"], "echo hi");
    assert_eq!(reply.content, "working");
    assert_eq!(reply.reasoning.as_deref(), Some("think "));
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
    assert_eq!(request["reasoning"]["effort"], "high");
    assert_eq!(request["extra_body"]["provider_flag"], true);
    assert_eq!(request["extra_body"]["thinking"]["type"], "enabled");
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
    let client = ConfiguredModelClient::from_yaml(&catalog(&endpoint), agent()).unwrap();
    let reply = client
        .summarize(
            ModelSelection {
                model: "chat".to_string(),
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
    let client = ConfiguredModelClient::from_yaml(&catalog(&endpoint), agent()).unwrap();
    let reply = client
        .complete(
            ModelSelection {
                model: "chat".to_string(),
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
    let client = ConfiguredModelClient::from_yaml(&catalog(&endpoint), agent()).unwrap();
    client
        .complete(
            ModelSelection {
                model: "chat".to_string(),
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
        ConfiguredModelClient::from_yaml(&catalog(&format!("http://{address}")), agent()).unwrap();
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
                model: "chat".to_string(),
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
fn config_rejects_reserved_body_overrides() {
    let invalid = r#"
providers:
  local:
    endpoint: http://localhost:1/responses
    models:
      test:
        contextWindowTokens: 100000
        maxOutputTokens: 4096
        body:
          input: []
"#;
    let error = ModelCatalog::from_yaml(invalid).unwrap_err();
    assert!(error.to_string().contains("reserved field input"));
}

#[test]
fn builtin_catalog_resolves_two_models_through_one_shared_provider() {
    let catalog = ModelCatalog::builtin().unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelName: deepseek-v4-pro
providers:
  deepseek:
    type: deepseek
models:
  - modelName: deepseek-v4-pro
    provider: deepseek
    modelId: deepseek-v4-pro
  - modelName: deepseek-v4-flash
    provider: deepseek
    modelId: deepseek-v4-flash
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();

    assert_eq!(resolved.providers.len(), 1);
    assert_eq!(resolved.models.len(), 2);
    let model = &resolved.models["deepseek-v4-pro"];
    assert_eq!(model.context_window_tokens, 1_000_000);
    assert_eq!(model.max_output_tokens, 384_000);
    assert_eq!(model.max_input_tokens().unwrap(), 616_000);

    let client = ConfiguredModelClient::new(&catalog, &agent).unwrap();
    let limits = client.model_limits("deepseek-v4-pro").unwrap();
    assert_eq!(limits.max_input_tokens, 616_000);
    assert_eq!(limits.compact_trigger_tokens, 308_000);
    client
        .validate_selection(&ModelSelection {
            model: "deepseek-v4-flash".to_string(),
            reasoning: Some("high".to_string()),
        })
        .unwrap();
}

#[test]
fn resolved_models_and_reasoning_preserve_config_order() {
    let catalog = ModelCatalog::from_yaml(
        r#"
providers:
  local:
    endpoint: https://example.com/v1/responses
    models:
      test-model:
        contextWindowTokens: 100000
        maxOutputTokens: 4096
        reasoning:
          max:
            reasoning:
              effort: max
          auto: {}
          nonthink:
            reasoning:
              effort: none
"#,
    )
    .unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelName: model-z
providers:
  local:
    type: local
models:
  - modelName: model-z
    provider: local
    modelId: test-model
  - modelName: model-a
    provider: local
    modelId: test-model
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();

    let model_ids: Vec<&str> = resolved.models.keys().map(String::as_str).collect();
    assert_eq!(model_ids, ["model-z", "model-a"]);

    let reasoning: Vec<&str> = resolved.models["model-z"]
        .reasoning
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(reasoning, ["max", "auto", "nonthink"]);
}

#[test]
fn builtin_openai_provider_exposes_verified_model_capabilities() {
    let catalog = ModelCatalog::builtin().unwrap();
    let openai = &catalog.providers["openai"];
    assert_eq!(openai.endpoint, "https://api.openai.com/v1/responses");

    for id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.5", "gpt-5.4"] {
        let model = &openai.models[id];
        assert_eq!(model.context_window_tokens, 1_050_000, "{id}");
        assert_eq!(model.max_output_tokens, 128_000, "{id}");
        assert!(model.capabilities.image_input, "{id}");
        assert!(model.capabilities.tool_calls, "{id}");
        assert_eq!(model.default_reasoning_mode, "medium", "{id}");
        for effort in ["low", "medium", "high", "xhigh"] {
            assert_eq!(
                model.reasoning[effort]["reasoning"]["effort"], effort,
                "{id}"
            );
        }
    }

    for id in ["gpt-5.6-sol", "gpt-5.6-terra"] {
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            let reasoning = &openai.models[id].reasoning[effort]["reasoning"];
            assert_eq!(reasoning["effort"], effort, "{id}/{effort}");
            assert_eq!(reasoning["summary"], "auto", "{id}/{effort}");
        }
    }
    for id in ["gpt-5.5", "gpt-5.4"] {
        assert!(!openai.models[id].reasoning.contains_key("max"), "{id}");
    }
}

#[test]
fn openai_provider_instance_only_overrides_endpoint_and_credentials() {
    let catalog = ModelCatalog::builtin().unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelName: gpt-5.6-terra
providers:
  relay:
    type: openai
    baseUrl: https://relay.example.com/v1/responses
    apiKeyEnv: RELAY_API_KEY
models:
  - modelName: gpt-5.6-terra
    provider: relay
    modelId: gpt-5.6-terra
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();

    let provider = &resolved.providers["relay"];
    assert_eq!(provider.endpoint, "https://relay.example.com/v1/responses");
    assert_eq!(provider.api_key_env.as_deref(), Some("RELAY_API_KEY"));
    let model = &resolved.models["gpt-5.6-terra"];
    assert!(model.capabilities.image_input);
    assert_eq!(model.default_reasoning_mode, "medium");
}

#[test]
fn custom_provider_directory_adds_one_provider_per_yaml_file() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("newapi.yaml"),
        r#"
protocol: open_ai_responses
endpoint: https://gateway.example.com/v1/responses
models:
  chat:
    contextWindowTokens: 100000
    maxOutputTokens: 4096
    capabilities:
      imageInput: true
      toolCalls: true
    defaultReasoningMode: medium
    reasoning:
      medium:
        reasoning:
          effort: medium
"#,
    )
    .unwrap();

    let mut catalog = ModelCatalog::builtin().unwrap();
    catalog.merge_provider_directory(directory.path()).unwrap();
    assert!(catalog.providers.contains_key("newapi"));

    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelName: chat
providers:
  relay:
    type: newapi
models:
  - modelName: chat
    provider: relay
    modelId: chat
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();
    assert_eq!(
        resolved.providers["relay"].endpoint,
        "https://gateway.example.com/v1/responses"
    );
}

#[test]
fn custom_provider_directory_rejects_builtin_name_collisions() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("openai.yaml"),
        "endpoint: https://example.com/v1/responses\nmodels: {}\n",
    )
    .unwrap();
    let error = ModelCatalog::builtin()
        .unwrap()
        .merge_provider_directory(directory.path())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("conflicts with a built-in provider")
    );
}

#[test]
fn profile_overrides_only_base_url_credentials_and_model_limits() {
    let catalog = ModelCatalog::builtin().unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelName: custom-pro
providers:
  deepseek:
    type: deepseek
    baseUrl: https://gateway.example.com/responses
    apiKeyEnv: CUSTOM_DEEPSEEK_KEY
models:
  - modelName: custom-pro
    provider: deepseek
    modelId: deepseek-v4-pro
    contextWindowTokens: 800000
    maxOutputTokens: 200000
    compactThreshold: 0.6
    defaultReasoningMode: max
"#,
    )
    .unwrap();
    let resolved = ModelClientConfig::resolve(&catalog, &agent).unwrap();

    let provider = &resolved.providers["deepseek"];
    assert_eq!(provider.endpoint, "https://gateway.example.com/responses");
    assert_eq!(provider.api_key_env.as_deref(), Some("CUSTOM_DEEPSEEK_KEY"));
    assert_eq!(provider.request.max_retries, 4);

    let model = &resolved.models["custom-pro"];
    assert_eq!(model.context_window_tokens, 800_000);
    assert_eq!(model.max_output_tokens, 200_000);
    assert_eq!(model.max_input_tokens().unwrap(), 600_000);
    assert_eq!(model.compact_threshold, 0.6);
    assert_eq!(model.default_reasoning_mode, "max");
}

#[test]
fn reasoning_cannot_override_the_model_output_limit() {
    let invalid = r#"
providers:
  local:
    endpoint: http://localhost:1/responses
    models:
      test:
        contextWindowTokens: 100000
        maxOutputTokens: 4096
        reasoning:
          high:
            max_tokens: 99999
"#;

    let error = ModelCatalog::from_yaml(invalid).unwrap_err();
    assert!(error.to_string().contains("reserved field max_tokens"));
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
    let client = ConfiguredModelClient::from_yaml(&catalog(&endpoint), agent()).unwrap();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let messages = vec![
        ContextMessage::system("system"),
        ContextMessage::user("hello"),
    ];
    let cancellation = CancellationToken::new();
    let error = client
        .stream_turn(
            ModelSelection {
                model: "chat".to_string(),
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
