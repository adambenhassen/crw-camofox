//! A caller-supplied LLM `baseUrl` receives page content and the caller's key
//! from the server, so it is refused when it points at a private address.
//!
//! Its own test binary: other server tests set `CRW_ALLOW_LOOPBACK_FOR_TESTS`,
//! which switches the private-address checks off process-wide.

use axum_test::TestServer;
use crw_core::config::AppConfig;
use crw_server::app::create_app;
use crw_server::state::AppState;
use serde_json::{Value, json};

fn test_app() -> TestServer {
    let config: AppConfig = toml::from_str(
        r#"
[search]
enabled = true

[renderer]
mode = "none"

[renderer.camofox]
base_url = "http://127.0.0.1:9"
"#,
    )
    .unwrap();
    TestServer::new(create_app(
        AppState::new(config).expect("AppState::new failed"),
    ))
}

fn json_format() -> Value {
    json!({ "type": "json", "schema": { "type": "object", "properties": { "title": { "type": "string" } } } })
}

async fn assert_refused(resp: axum_test::TestResponse) {
    assert_eq!(resp.status_code(), 400, "{}", resp.text());
    let body: Value = resp.json();
    let error = body["error"].as_str().unwrap_or_default();
    assert!(error.contains("baseUrl"), "{error}");
}

#[tokio::test]
async fn v1_scrape_refuses_a_private_base_url() {
    let app = test_app();
    let resp = app
        .post("/v1/scrape")
        .json(&json!({
            "url": "https://93.184.215.14/",
            "formats": ["markdown", "json"],
            "jsonSchema": { "type": "object" },
            "llmApiKey": "k",
            "llmProvider": "openai",
            "baseUrl": "http://127.0.0.1:18777/v1",
        }))
        .await;
    assert_refused(resp).await;
}

#[tokio::test]
async fn v2_scrape_refuses_a_metadata_base_url() {
    let app = test_app();
    let resp = app
        .post("/v2/scrape")
        .json(&json!({
            "url": "https://93.184.215.14/",
            "formats": ["markdown", json_format()],
            "llmApiKey": "k",
            "llmProvider": "openai",
            "baseUrl": "http://169.254.169.254/v1",
        }))
        .await;
    assert_refused(resp).await;
}

#[tokio::test]
async fn search_refuses_a_private_base_url() {
    let app = test_app();
    let resp = app
        .post("/v1/search")
        .json(&json!({
            "query": "rust",
            "answer": true,
            "llmApiKey": "k",
            "llmProvider": "openai",
            "baseUrl": "http://10.0.0.5/v1",
        }))
        .await;
    assert_refused(resp).await;
}

/// A bad template is one 400 at batch start, not one failed document per URL.
#[tokio::test]
async fn v2_batch_refuses_a_private_base_url_at_start() {
    let app = test_app();
    let resp = app
        .post("/v2/batch/scrape")
        .json(&json!({
            "urls": ["https://93.184.215.14/"],
            "formats": ["markdown", json_format()],
            "llmApiKey": "k",
            "llmProvider": "openai",
            "baseUrl": "http://192.168.1.10/v1",
        }))
        .await;
    assert_refused(resp).await;
}

/// A public base URL passes the check (it fails later, at the provider call).
#[tokio::test]
async fn public_base_url_is_not_refused() {
    let req: crw_core::types::ScrapeRequest = serde_json::from_value(json!({
        "url": "https://93.184.215.14/",
        "llmApiKey": "k",
        "baseUrl": "https://1.1.1.1/v1",
    }))
    .unwrap();
    crw_crawl::single::validate_byok_base_url(&req)
        .await
        .expect("a public base URL is allowed");
}
