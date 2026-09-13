#![cfg(feature = "camofox")]
//! The camofox tier must not return a page whose final URL is internal.
//!
//! Own test binary: `camofox_tests.rs` sets `CRW_ALLOW_LOOPBACK_FOR_TESTS`,
//! which disables the address checks process-wide and would race this test.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::Path;
use axum::routing::{delete, post};
use axum::{Json, Router};
use crw_core::Deadline;
use crw_renderer::camofox::CamofoxRenderer;
use crw_renderer::traits::PageFetcher;
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn create_tab(Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({ "ok": true, "tabId": "tab-1", "sessionKey": "s-1" }))
}

async fn ok(Path(_id): Path<String>, Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({ "ok": true }))
}

fn deadline() -> Deadline {
    Deadline::now_plus(Duration::from_secs(30))
}

/// The page ended up somewhere internal: a redirect or JS navigation to the
/// metadata endpoint. Camofox renders it; crw must not return it.
async fn evaluate_internal_final_url(
    Path(_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    if body["expression"].as_str() == Some("location.href") {
        return Json(json!({
            "ok": true, "result": "http://169.254.169.254/latest/meta-data/", "resultType": "string", "truncated": false
        }));
    }
    Json(
        json!({ "ok": true, "result": "<html><body>ami-id instance-id</body></html>", "resultType": "string", "truncated": false }),
    )
}

async fn evaluate_private_final_url(
    Path(_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    if body["expression"].as_str() == Some("location.href") {
        return Json(json!({
            "ok": true, "result": "http://10.0.0.5:9377/tabs", "resultType": "string", "truncated": false
        }));
    }
    Json(
        json!({ "ok": true, "result": "<html><body>internal service</body></html>", "resultType": "string", "truncated": false }),
    )
}

async fn spawn_mock_with_evaluate<H, T>(handler: H) -> String
where
    H: axum::handler::Handler<T, ()>,
    T: 'static,
{
    let app = Router::new()
        .route("/tabs", post(create_tab))
        .route("/tabs/{id}/navigate", post(ok))
        .route("/tabs/{id}/wait", post(ok))
        .route("/tabs/{id}/evaluate", post(handler))
        .route("/tabs/{id}", delete(ok));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn internal_final_url_is_refused_not_rendered() {
    for base in [
        spawn_mock_with_evaluate(evaluate_internal_final_url).await,
        spawn_mock_with_evaluate(evaluate_private_final_url).await,
    ] {
        let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(10));
        let res = renderer
            .fetch(
                "https://example.com/redirects",
                &HashMap::new(),
                None,
                deadline(),
            )
            .await;
        match res {
            Err(e) => assert!(
                e.to_string().contains("blocked destination"),
                "expected the outbound refusal, got {e}"
            ),
            Ok(r) => panic!(
                "an internal final URL must not be rendered, got {:?}",
                r.html
            ),
        }
    }
}
