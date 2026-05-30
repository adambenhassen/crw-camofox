//! Integration tests for the change-tracking primitives: the stateless
//! `POST /v1/change-tracking/diff` endpoint (single + batch), the
//! `changeTracking` scrape format wire-shape lock (plain string, not an
//! object), and the `/v1/capabilities` advertisement.

use axum_test::TestServer;
use crw_core::config::AppConfig;
use crw_server::app::create_app;
use crw_server::state::AppState;
use serde_json::json;

fn test_app() -> TestServer {
    let config: AppConfig = toml::from_str("").unwrap();
    let state = AppState::new(config).expect("AppState::new failed");
    TestServer::new(create_app(state))
}

#[tokio::test]
async fn diff_single_gitdiff_reports_changed() {
    let server = test_app();
    let resp = server
        .post("/v1/change-tracking/diff")
        .json(&json!({
            "modes": ["gitDiff"],
            "previous": { "markdown": "Starter $19", "contentHash": "x" },
            "current": { "markdown": "Starter $24" }
        }))
        .await;
    resp.assert_status_ok();
    let j: serde_json::Value = resp.json();
    assert_eq!(j["success"], true);
    assert_eq!(j["data"]["status"], "changed");
    assert_eq!(j["data"]["firstObservation"], false);
    assert!(
        j["data"]["diff"]["text"]
            .as_str()
            .unwrap()
            .contains("+Starter $24"),
        "unified diff should contain the new line"
    );
    // gitDiff-only => diff.json carries the parse-diff AST
    assert!(j["data"]["diff"]["json"]["files"].is_array());
}

#[tokio::test]
async fn diff_single_first_observation_when_no_previous() {
    let server = test_app();
    let resp = server
        .post("/v1/change-tracking/diff")
        .json(&json!({
            "modes": ["gitDiff"],
            "current": { "markdown": "# Brand new page" }
        }))
        .await;
    resp.assert_status_ok();
    let j: serde_json::Value = resp.json();
    assert_eq!(j["data"]["status"], "changed");
    assert_eq!(j["data"]["firstObservation"], true);
    assert!(j["data"].get("diff").is_none() || j["data"]["diff"].is_null());
    assert!(j["data"]["snapshot"]["contentHash"].is_string());
}

#[tokio::test]
async fn diff_single_identical_is_same() {
    let server = test_app();
    let resp = server
        .post("/v1/change-tracking/diff")
        .json(&json!({
            "modes": ["gitDiff"],
            "previous": { "markdown": "# Hello\n\nbody", "contentHash": "x" },
            "current": { "markdown": "# Hello\n\nbody" }
        }))
        .await;
    resp.assert_status_ok();
    let j: serde_json::Value = resp.json();
    assert_eq!(j["data"]["status"], "same");
}

#[tokio::test]
async fn diff_json_mode_per_field() {
    let server = test_app();
    let resp = server
        .post("/v1/change-tracking/diff")
        .json(&json!({
            "modes": ["json"],
            "previous": { "json": {"price": "$19"}, "contentHash": "x" },
            "current": { "json": {"price": "$24"} }
        }))
        .await;
    resp.assert_status_ok();
    let j: serde_json::Value = resp.json();
    assert_eq!(j["data"]["status"], "changed");
    assert_eq!(
        j["data"]["diff"]["json"]["price"],
        json!({"previous": "$19", "current": "$24"})
    );
    // json mode has no text surface
    assert!(j["data"]["diff"].get("text").is_none() || j["data"]["diff"]["text"].is_null());
}

#[tokio::test]
async fn diff_batch_returns_array_and_applies_shared_modes() {
    let server = test_app();
    // Shared top-level `modes`; items omit their own.
    let resp = server
        .post("/v1/change-tracking/diff")
        .json(&json!({
            "modes": ["gitDiff"],
            "batch": [
                { "url": "https://a.com", "previous": {"markdown": "a", "contentHash": "x"}, "current": {"markdown": "a"} },
                { "url": "https://b.com", "previous": {"markdown": "b", "contentHash": "y"}, "current": {"markdown": "B changed"} }
            ]
        }))
        .await;
    resp.assert_status_ok();
    let j: serde_json::Value = resp.json();
    assert_eq!(j["success"], true);
    let data = j["data"].as_array().expect("batch returns an array");
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["status"], "same");
    assert_eq!(data[1]["status"], "changed");
}

#[tokio::test]
async fn diff_discriminator_single_body_with_extra_fields_decodes_as_single() {
    // A single body that ALSO carries fields a batch might use (no `batch`
    // key) must decode as single — no deny_unknown_fields rejection.
    let server = test_app();
    let resp = server
        .post("/v1/change-tracking/diff")
        .json(&json!({
            "modes": ["gitDiff"],
            "tag": "target-1",
            "previous": { "markdown": "old", "contentHash": "x" },
            "current": { "markdown": "new" }
        }))
        .await;
    resp.assert_status_ok();
    let j: serde_json::Value = resp.json();
    assert_eq!(j["data"]["status"], "changed");
    assert_eq!(j["data"]["tag"], "target-1");
}

#[tokio::test]
async fn diff_empty_batch_is_bad_request() {
    let server = test_app();
    let resp = server
        .post("/v1/change-tracking/diff")
        .json(&json!({ "modes": ["gitDiff"], "batch": [] }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn scrape_object_format_entry_is_rejected() {
    // Wire-shape regression lock: the `changeTracking` format MUST be the plain
    // string in formats[]; an object entry fails OutputFormat deserialization.
    let server = test_app();
    let resp = server
        .post("/v1/scrape")
        .json(&json!({
            "url": "https://example.com",
            "formats": [{ "type": "changeTracking" }]
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    let j: serde_json::Value = resp.json();
    assert_eq!(j["success"], false);
}

#[tokio::test]
async fn scrape_unknown_format_string_is_rejected() {
    let server = test_app();
    let resp = server
        .post("/v1/scrape")
        .json(&json!({
            "url": "https://example.com",
            "formats": ["definitelyNotAFormat"]
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn capabilities_advertise_change_tracking() {
    let server = test_app();
    let resp = server.get("/v1/capabilities").await;
    resp.assert_status_ok();
    let j: serde_json::Value = resp.json();
    let supported = j["formats"]["supported"].as_array().unwrap();
    assert!(
        supported.iter().any(|v| v == "changeTracking"),
        "capabilities must advertise changeTracking"
    );
    let modes = j["formats"]["changeTrackingModes"].as_array().unwrap();
    assert!(modes.iter().any(|v| v == "gitDiff"));
    assert!(modes.iter().any(|v| v == "json"));
}
