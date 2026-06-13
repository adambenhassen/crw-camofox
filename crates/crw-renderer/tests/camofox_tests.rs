#![cfg(feature = "camofox")]
//! Behavioural tests for the Camofox (camofox-browser REST) renderer tier.
//! A small axum app emulates the camofox-browser `:9377` REST surface so we can
//! assert the navigate→wait→evaluate→close round-trip and `FetchResult` mapping
//! without a live Firefox.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::Path;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use crw_core::Deadline;
use crw_renderer::camofox::CamofoxRenderer;
use crw_renderer::traits::PageFetcher;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const RENDERED_HTML: &str = "<html><body><h1>camofox rendered</h1></body></html>";

async fn create_tab(Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({ "ok": true, "tabId": "tab-1", "sessionKey": "s-1" }))
}

async fn wait(Path(_id): Path<String>, Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn evaluate(Path(_id): Path<String>, Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "result": RENDERED_HTML,
        "resultType": "string",
        "truncated": false,
    }))
}

async fn close_tab(Path(_id): Path<String>, Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "engine": "camoufox", "browserConnected": true }))
}

async fn spawn_camofox_mock() -> String {
    let app = Router::new()
        .route("/tabs", post(create_tab))
        .route("/tabs/{id}/wait", post(wait))
        .route("/tabs/{id}/evaluate", post(evaluate))
        .route("/tabs/{id}", delete(close_tab))
        .route("/health", get(health));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn deadline() -> Deadline {
    Deadline::now_plus(Duration::from_secs(30))
}

#[tokio::test]
async fn fetch_returns_evaluated_html() {
    let base = spawn_camofox_mock().await;
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(10));

    let result = renderer
        .fetch(
            "https://example.com",
            &HashMap::new(),
            None,
            deadline(),
        )
        .await
        .expect("camofox fetch should succeed against the mock");

    assert_eq!(result.status_code, 200);
    assert!(
        result.html.contains("camofox rendered"),
        "expected evaluated outerHTML, got: {}",
        result.html
    );
    assert_eq!(result.rendered_with.as_deref(), Some("camofox"));
}

#[tokio::test]
async fn name_and_js_support() {
    let renderer = CamofoxRenderer::new("camofox", "http://127.0.0.1:1", None, Duration::from_secs(5));
    assert_eq!(renderer.name(), "camofox");
    assert!(renderer.supports_js());
}

#[tokio::test]
async fn is_available_reads_health() {
    let base = spawn_camofox_mock().await;
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(5));
    assert!(renderer.is_available().await);
}

#[tokio::test]
async fn fetch_fails_when_deadline_expired() {
    let renderer = CamofoxRenderer::new("camofox", "http://127.0.0.1:1", None, Duration::from_secs(5));
    let expired = Deadline::now_plus(Duration::from_millis(0));
    let res = renderer
        .fetch("https://example.com", &HashMap::new(), None, expired)
        .await;
    assert!(res.is_err(), "expired deadline should short-circuit before any HTTP call");
}
