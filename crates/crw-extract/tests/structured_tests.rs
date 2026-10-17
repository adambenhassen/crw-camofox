use crw_core::config::LlmConfig;
use crw_extract::structured::extract_structured_with_usage;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn parse_json_response_invalid_json() {
    // parse_json_response is private, so we test schema validation behavior.
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        },
        "required": ["name"]
    });
    // Missing required field "name" should fail validation
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&json!({}))
        .map(|e| e.to_string())
        .collect();
    assert!(!errors.is_empty(), "Missing required field should fail");
}

#[test]
fn validate_schema_empty_object() {
    // Empty object is valid if no required fields
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        }
    });
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&json!({}))
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "Empty object should be valid: {errors:?}"
    );
}

fn anthropic_llm(base_url: String) -> LlmConfig {
    LlmConfig {
        provider: "anthropic".into(),
        api_key: "test-key".into(),
        model: "claude-sonnet-5-5".into(),
        base_url: Some(base_url),
        ..Default::default()
    }
}

/// A reply that opens with thinking blocks must still yield its tool call, and
/// the request carries the low effort that keeps thinking inside `max_tokens`.
#[tokio::test]
async fn anthropic_thinking_blocks_before_tool_use_are_skipped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [
                { "type": "thinking", "thinking": "", "signature": "c2ln" },
                { "type": "redacted_thinking", "data": "ZGF0YQ==" },
                { "type": "tool_use", "id": "t1", "name": "extract_data", "input": { "name": "P" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 20, "output_tokens": 10 }
        })))
        .mount(&server)
        .await;

    let llm = anthropic_llm(format!("{}/v1", server.uri()));
    let schema = json!({ "type": "object", "properties": { "name": { "type": "string" } } });
    let out = extract_structured_with_usage("content", &schema, &llm, None)
        .await
        .unwrap();
    assert_eq!(out.value, json!({ "name": "P" }));

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["output_config"], json!({ "effort": "low" }));
}

#[tokio::test]
async fn anthropic_thinking_block_before_text_json_is_skipped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [
                { "type": "thinking", "thinking": "", "signature": "c2ln" },
                { "type": "text", "text": "{\"name\": \"Bob\"}" }
            ],
            "usage": { "input_tokens": 5, "output_tokens": 5 }
        })))
        .mount(&server)
        .await;

    let llm = anthropic_llm(format!("{}/v1", server.uri()));
    let schema = json!({ "type": "object", "properties": { "name": { "type": "string" } } });
    let value = crw_extract::structured::extract_structured("Bob.", &schema, &llm)
        .await
        .unwrap();
    assert_eq!(value, json!({ "name": "Bob" }));
}

/// A model slower than `[extraction.llm] timeout_ms` fails with an error that
/// names the key, on both transports. The pooled client's fixed 60 s bound would
/// otherwise let the 1.5 s answer through.
#[tokio::test]
async fn timeout_ms_bounds_extraction_and_names_itself() {
    let server = MockServer::start().await;
    let answer = json!({ "name": "Ada" });
    let openai = json!({
        "choices": [{ "message": { "tool_calls": [{
            "id": "c1", "type": "function",
            "function": { "name": "extract_data", "arguments": answer.to_string() }
        }] } }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 }
    });
    let anthropic = json!({
        "content": [{ "type": "tool_use", "id": "t1", "name": "extract_data", "input": answer }],
        "usage": { "input_tokens": 5, "output_tokens": 5 }
    });
    for (route, body) in [
        ("/v1/chat/completions", openai),
        ("/v1/messages", anthropic),
    ] {
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(body)
                    .set_delay(std::time::Duration::from_millis(1500)),
            )
            .mount(&server)
            .await;
    }
    let schema = json!({ "type": "object", "properties": { "name": { "type": "string" } } });
    let base = format!("{}/v1", server.uri());
    for llm in [
        LlmConfig {
            provider: "openai".into(),
            ..anthropic_llm(base.clone())
        },
        anthropic_llm(base.clone()),
    ] {
        let provider = llm.provider.clone();
        let slow = LlmConfig {
            timeout_ms: 1000,
            ..llm.clone()
        };
        let err = crw_extract::structured::extract_structured("Ada.", &schema, &slow)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("timed out after 1s") && err.contains("timeout_ms"),
            "{provider}: {err}"
        );

        // Control: a budget above the model's latency still answers.
        let roomy = LlmConfig {
            timeout_ms: 10_000,
            ..llm
        };
        let value = crw_extract::structured::extract_structured("Ada.", &schema, &roomy)
            .await
            .unwrap_or_else(|e| panic!("{provider}: {e}"));
        assert_eq!(value, answer, "{provider}");
    }
}
