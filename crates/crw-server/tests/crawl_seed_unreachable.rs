//! A crawl that could not fetch a single page must end Failed with a reason,
//! not `success:true, completed:0, error:None`, which reads as "the site has
//! no pages".
//!
//! Own test binary because it sets `CRW_ALLOW_LOOPBACK_FOR_TESTS`, which is
//! process-global.

use std::time::Duration;

use crw_core::config::{AppConfig, RendererMode};
use crw_core::types::{CrawlRequest, CrawlStatus};
use crw_server::state::AppState;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn crawl_whose_seed_hits_a_dead_origin_fails() {
    // SAFETY: one binary per tests/*.rs, set before any fetcher is built.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") };

    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(522)
                .insert_header("content-type", "text/html")
                .set_body_string("<html><body><h1>Connection timed out</h1></body></html>"),
        )
        .mount(&origin)
        .await;

    let mut cfg = AppConfig::default();
    cfg.renderer.mode = RendererMode::None;
    cfg.crawler.respect_robots_txt = false;
    let state = AppState::new(cfg).expect("AppState");

    let req: CrawlRequest =
        serde_json::from_value(serde_json::json!({ "url": format!("{}/", origin.uri()) }))
            .expect("crawl request");
    let id = state.start_crawl_job(req).await;

    let mut last = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let jobs = state.crawl_jobs.read().await;
        let s = jobs.get(&id).expect("crawl job").rx.borrow().clone();
        if !matches!(s.status, CrawlStatus::InProgress) {
            last = Some(s);
            break;
        }
    }
    let s = last.expect("crawl finished");
    assert!(
        matches!(s.status, CrawlStatus::Failed) && !s.success,
        "got status {:?} success {}",
        s.status,
        s.success
    );
    assert!(
        s.error.as_deref().is_some_and(|e| e.contains("522")),
        "the failure must name the cause, got {:?}",
        s.error
    );
}
