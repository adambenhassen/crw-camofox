//! An extract job over a page that is an origin error or a wall must fail that
//! URL, not report Completed with empty data and charge for it. `scrape_url`
//! skips the LLM call for such pages, so without the job-level check the
//! empty `Ok` looked like a successful extraction.
//!
//! Own test binary because it sets `CRW_ALLOW_LOOPBACK_FOR_TESTS`, which is
//! process-global.

use std::time::Duration;

use crw_core::config::{AppConfig, RendererMode};
use crw_core::types::ScrapeRequest;
use crw_server::state::{AppState, ExtractStatus};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn extract_job_fails_an_origin_error_page_instead_of_completing() {
    // SAFETY: one binary per tests/*.rs, set before any fetcher is built.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") };

    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(404)
                .insert_header("content-type", "text/html")
                .set_body_string("<html><body><h1>Not Found</h1></body></html>"),
        )
        .mount(&origin)
        .await;

    let mut cfg = AppConfig::default();
    cfg.renderer.mode = RendererMode::None;
    let state = AppState::new(cfg).expect("AppState");

    let url = format!("{}/missing", origin.uri());
    let template: ScrapeRequest = serde_json::from_value(serde_json::json!({
        "url": url,
        "formats": ["json"],
        "jsonSchema": {"type": "object", "properties": {"title": {"type": "string"}}},
    }))
    .expect("template");

    let id = state.start_extract_job(vec![url], template).await;

    let mut status = ExtractStatus::Processing;
    let mut error = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let jobs = state.extract_jobs.read().await;
        let rec = jobs.get(&id).expect("job record");
        if !matches!(rec.status, ExtractStatus::Processing) {
            status = rec.status;
            error = rec.error.clone();
            break;
        }
    }

    assert!(
        matches!(status, ExtractStatus::Failed),
        "an origin error page must fail the extract job, got {status:?} (error {error:?})"
    );
    assert!(error.is_some(), "the failure must carry a reason");
}
