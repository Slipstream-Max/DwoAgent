use dwo_context::{ContextManager, ContextMessage, SessionContext, ToolResultRecord};
use dwo_model_client::{
    AgentModelConfig, ConfiguredModelClient, ModelCatalog, ModelClient, ModelSelection,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn selection(model: &str) -> ModelSelection {
    ModelSelection {
        model: model.to_string(),
        reasoning: None,
    }
}

async fn stream(
    client: &ConfiguredModelClient,
    model: &str,
    messages: &[ContextMessage],
    tools: &[serde_json::Value],
) -> dwo_model_client::ModelReply {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    client
        .stream_turn(
            selection(model),
            messages,
            tools,
            events,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY and consumes API quota"]
async fn deepseek_responses_supports_hosted_search_and_local_function_round_trip() {
    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelId: deepseek-v4-flash
providers:
  deepseek:
    type: deepseek
    apiKeyEnv: DEEPSEEK_API_KEY
models:
  - modelName: deepseek-v4-flash
    provider: deepseek
    modelId: deepseek-v4-flash
"#,
    )
    .unwrap();
    let client = ConfiguredModelClient::new(&ModelCatalog::builtin().unwrap(), &agent).unwrap();

    let searched = stream(
        &client,
        "deepseek-v4-flash",
        &[
            ContextMessage::system("Use the hosted web search tool when explicitly requested."),
            ContextMessage::user(
                "Search the web for the current DeepSeek API docs homepage title. Reply briefly.",
            ),
        ],
        &[],
    )
    .await;
    assert!(!searched.content.trim().is_empty());
    assert!(
        searched
            .remote_tool_calls
            .iter()
            .any(|call| call["name"] == "web_search")
    );

    let tools = [json!({
        "type":"function",
        "function":{
            "name":"lookup_code",
            "description":"Look up a short code. Always call this function when asked for a code.",
            "parameters":{
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"],
                "additionalProperties":false
            },
            "strict":true
        }
    })];
    let mut messages = vec![
        ContextMessage::system("You must call lookup_code for code lookup requests."),
        ContextMessage::user("Use lookup_code to get the code for alpha."),
    ];
    let called = stream(&client, "deepseek-v4-flash", &messages, &tools).await;
    assert_eq!(called.tool_calls.len(), 1);
    let call_id = called.tool_calls[0]["id"].as_str().unwrap().to_string();
    let mut context = ContextManager::new(SessionContext {
        messages,
        ..SessionContext::default()
    });
    context.append_response_items("deepseek", called.context_output_items());
    context.append_tool(ToolResultRecord {
        tool_call_id: call_id,
        tool_name: "lookup_code".to_string(),
        output: json!({"code":"A-17"}),
        model_context: Vec::new(),
    });
    messages = context.into_context().messages;
    let completed = stream(&client, "deepseek-v4-flash", &messages, &tools).await;
    assert!(completed.content.contains("A-17"));
}

#[tokio::test]
#[ignore = "requires NEW_API_KEY and consumes API quota"]
async fn newapi_responses_supports_hosted_web_search() {
    let agent = AgentModelConfig::from_yaml(
        r#"
defaultModelId: gpt-5.6-sol
providers:
  newapi:
    type: openai
    baseUrl: https://api.kkrich.ltd/v1/responses
    apiKeyEnv: NEW_API_KEY
models:
  - modelName: gpt-5.6-sol
    provider: newapi
    modelId: gpt-5.6-sol
"#,
    )
    .unwrap();
    let client = ConfiguredModelClient::new(&ModelCatalog::builtin().unwrap(), &agent).unwrap();
    let reply = stream(
        &client,
        "gpt-5.6-sol",
        &[
            ContextMessage::system("Use web search when explicitly requested."),
            ContextMessage::user(
                "Search the web for the current DeepSeek API docs homepage title. Reply briefly.",
            ),
        ],
        &[],
    )
    .await;
    assert!(!reply.content.trim().is_empty());
    assert!(
        reply
            .remote_tool_calls
            .iter()
            .any(|call| call["name"] == "web_search")
    );
}
