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
        reasoning:
          high:
            reasoning_effort: high
            extra_body:
              thinking:
                type: enabled
"#
    )
}

fn agent() -> &'static str {
    r#"
defaultModelId: chat
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
        json!({"choices":[{"delta":{"reasoning_content":"think "}}]}).to_string(),
        json!({"choices":[{"delta":{"content":"working"}}]}).to_string(),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"terminal","arguments":"{\"action\":\"run\",\"comm"}}]}}]}).to_string(),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}).to_string(),
        json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}}).to_string(),
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
    assert_eq!(limits.max_input_tokens, 85_904);
    assert_eq!(limits.compact_trigger_tokens, 68_723);
    assert_eq!(client.default_model_id(), "chat");
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let reply = client
        .stream_turn(
            ModelSelection {
                model: "chat".to_string(),
                reasoning: Some("high".to_string()),
            },
            vec![
                ContextMessage::system("system prompt"),
                ContextMessage::user(MessageContent::blocks(vec![
                    ContentBlock::text("hello"),
                    ContentBlock::image("image/png", "aGVsbG8="),
                ])),
            ],
            vec![json!({"type":"function","function":{"name":"terminal"}})],
            events_tx,
            CancellationToken::new(),
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
    assert_eq!(reply.content, "working");
    assert_eq!(reply.reasoning.as_deref(), Some("think "));
    assert_eq!(reply.finish_reason, FinishReason::ToolCalls);
    assert_eq!(reply.tool_calls[0]["id"], "call-1");
    assert_eq!(reply.tool_calls[0]["name"], "terminal");
    assert_eq!(reply.tool_calls[0]["arguments"]["command"], "echo hi");
    assert_eq!(reply.usage.total_tokens, 14);

    let request = request_rx.await.unwrap();
    assert_eq!(request["stream"], true);
    assert_eq!(request["model"], "test-model");
    assert_eq!(request["max_tokens"], 4096);
    assert_eq!(request["messages"][0]["role"], "system");
    assert_eq!(request["messages"][1]["content"][0]["type"], "text");
    assert_eq!(
        request["messages"][1]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );
    assert_eq!(request["tools"].as_array().unwrap().len(), 1);
    assert_eq!(request["reasoning_effort"], "high");
    assert_eq!(request["extra_body"]["provider_flag"], true);
    assert_eq!(request["extra_body"]["thinking"]["type"], "enabled");
}

#[tokio::test]
async fn summary_uses_non_streaming_request_without_tools() {
    let payload = json!({
        "choices":[{"message":{"role":"assistant","content":"summary text"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":20,"completion_tokens":3,"total_tokens":23}
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
    assert_eq!(request["messages"][0]["content"], "compact instruction");
    assert_eq!(request["messages"][1]["content"], "history");
}

#[tokio::test]
async fn completion_uses_non_streaming_request_without_tools() {
    let payload = json!({
        "choices":[{"message":{"role":"assistant","content":"Short title"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}
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
    assert_eq!(request["messages"][0]["content"], "Generate a title");
    assert_eq!(request["messages"][1]["content"], "Investigate flaky tests");
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
    let error = client
        .stream_turn(
            ModelSelection {
                model: "chat".to_string(),
                reasoning: None,
            },
            vec![
                ContextMessage::system("system"),
                ContextMessage::user("hello"),
            ],
            Vec::new(),
            events,
            cancellation,
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
    endpoint: http://localhost:1/chat/completions
    models:
      test:
        contextWindowTokens: 100000
        maxOutputTokens: 4096
        body:
          messages: []
"#;
    let error = ModelCatalog::from_yaml(invalid).unwrap_err();
    assert!(error.to_string().contains("reserved field messages"));
}

#[test]
fn builtin_catalog_resolves_two_models_through_one_shared_provider() {
    let catalog = ModelCatalog::builtin().unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelId: deepseek-v4-pro
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
    assert_eq!(model.max_input_tokens().unwrap(), 606_000);

    let client = ConfiguredModelClient::new(&catalog, &agent).unwrap();
    let limits = client.model_limits("deepseek-v4-pro").unwrap();
    assert_eq!(limits.max_input_tokens, 606_000);
    assert_eq!(limits.compact_trigger_tokens, 303_000);
    client
        .validate_selection(&ModelSelection {
            model: "deepseek-v4-flash".to_string(),
            reasoning: Some("high".to_string()),
        })
        .unwrap();
}

#[test]
fn profile_overrides_only_base_url_credentials_and_model_limits() {
    let catalog = ModelCatalog::builtin().unwrap();
    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelId: custom-pro
providers:
  deepseek:
    type: deepseek
    baseUrl: https://gateway.example.com/chat/completions
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
    assert_eq!(
        provider.endpoint,
        "https://gateway.example.com/chat/completions"
    );
    assert_eq!(provider.api_key_env.as_deref(), Some("CUSTOM_DEEPSEEK_KEY"));
    assert_eq!(provider.request.max_retries, 4);

    let model = &resolved.models["custom-pro"];
    assert_eq!(model.context_window_tokens, 800_000);
    assert_eq!(model.max_output_tokens, 200_000);
    assert_eq!(model.max_input_tokens().unwrap(), 590_000);
    assert_eq!(model.compact_threshold, 0.6);
    assert_eq!(model.default_reasoning_mode, "max");
}

#[test]
fn reasoning_cannot_override_the_model_output_limit() {
    let invalid = r#"
providers:
  local:
    endpoint: http://localhost:1/chat/completions
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
