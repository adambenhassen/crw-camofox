//! Crawl outcomes a caller bills from: a page that could not be fetched is
//! returned with its reason and counted `blocked` (dropping it made a crawl of a
//! dead site read as "the site has no pages"), and a walled page is counted
//! `blocked` with its challenge shell cleared.
//!
//! Own test binary because it sets `CRW_ALLOW_LOOPBACK_FOR_TESTS`, which is
//! process-global.

use std::time::Duration;

use crw_core::config::{AppConfig, RendererMode};
use crw_core::types::{CrawlRequest, CrawlStatus};
use crw_server::state::AppState;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn crawl_reports_a_dead_origin_page_instead_of_dropping_it() {
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
    // The page is reported, not dropped: one completed document the caller does
    // not pay for, carrying the reason.
    assert_eq!((s.completed, s.blocked), (1, 1), "{s:?}");
    let page = &s.data[0];
    assert_eq!(page.metadata.status_code, 522);
    assert_eq!(
        page.block.as_ref().map(|b| b.vendor.as_str()),
        Some(crw_core::types::HTTP_ERROR_VENDOR)
    );
    assert!(
        page.error.is_some(),
        "the failure must carry a reason: {page:?}"
    );
}

#[tokio::test]
async fn crawl_counts_a_walled_page_as_blocked_and_clears_it() {
    // SAFETY: one binary per tests/*.rs, set before any fetcher is built.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") };

    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("content-type", "text/html")
                .set_body_string(
                    "<html><body><script src=\"https://geo.captcha-delivery.com/captcha/\">\
                     </script><p>Please enable JS and disable any ad blocker</p></body></html>",
                ),
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
    assert_eq!((s.completed, s.blocked), (1, 1), "{s:?}");
    let page = &s.data[0];
    assert_eq!(
        page.block.as_ref().map(|b| b.vendor.as_str()),
        Some("datadome")
    );
    assert!(
        page.markdown.as_deref().unwrap_or_default().is_empty(),
        "the wall's text is not content: {:?}",
        page.markdown
    );
}
