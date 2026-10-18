//! A crawl neither reads nor writes the page cache.
//!
//! `run_crawl` fetches through the renderer directly, not through `scrape_url`,
//! so a crawled page is always live and never seeds an entry for a later
//! single scrape. These tests count what the ORIGIN received.

use std::sync::Arc;

use crw_core::Deadline;
use crw_core::config::{ExtractionConfig, RendererConfig, RendererMode, StealthConfig};
use crw_core::types::{CrawlRequest, CrawlState, CrawlStatus, OutputFormat, ScrapeRequest};
use crw_crawl::crawl::{CrawlOptions, run_crawl};
use crw_crawl::single::scrape_url;
use crw_renderer::FallbackRenderer;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const UA: &str = "crw-test";

/// wiremock binds to loopback, which the SSRF guard rejects by default.
fn allow_loopback() {
    // SAFETY: set before any fetch runs; every test in this file sets the same value.
    unsafe {
        std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1");
    }
}

fn renderer() -> Arc<FallbackRenderer> {
    allow_loopback();
    let cfg = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    Arc::new(FallbackRenderer::new(&cfg, UA, None, &StealthConfig::default()).expect("renderer"))
}

async fn origin(route: &str) -> (MockServer, String) {
    let para = "Lume Roasters sources single origin coffee from smallholder farms. ";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string(format!(
                    "<html><head><title>Lume</title></head><body><main><h1>Coffee</h1><p>{}</p></main></body></html>",
                    para.repeat(40)
                )),
        )
        .mount(&server)
        .await;
    let url = format!("{}{route}", server.uri());
    (server, url)
}

async fn hits(server: &MockServer, route: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == route)
        .count()
}

async fn scrape(r: &Arc<FallbackRenderer>, url: &str) -> crw_core::types::ScrapeData {
    let req = ScrapeRequest {
        url: url.to_string(),
        formats: vec![OutputFormat::Markdown],
        render_js: Some(false),
        ..Default::default()
    };
    scrape_url(
        &req,
        r,
        None,
        &ExtractionConfig::default(),
        UA,
        false,
        None,
        Deadline::from_request_ms(30_000),
    )
    .await
    .expect("scrape")
}

async fn crawl(r: &Arc<FallbackRenderer>, url: &str) {
    let id = Uuid::new_v4();
    let (state_tx, _rx) = tokio::sync::watch::channel(CrawlState {
        id,
        success: false,
        status: CrawlStatus::InProgress,
        total: 0,
        completed: 0,
        blocked: 0,
        data: Vec::new(),
        error: None,
    });
    run_crawl(CrawlOptions {
        id,
        req: CrawlRequest {
            url: url.to_string(),
            max_depth: Some(0),
            max_pages: Some(1),
            formats: vec![OutputFormat::Markdown],
            only_main_content: Some(false),
            json_schema: None,
            render_js: Some(false),
            wait_for: None,
            renderer: None,
            country: None,
            headers: std::collections::HashMap::new(),
        },
        renderer: r.clone(),
        max_concurrency: 1,
        respect_robots: false,
        requests_per_second: 100.0,
        user_agent: UA,
        state_tx,
        llm_config: None,
        proxy: None,
        jitter_factor: 0.0,
        deadline_ms_per_page: 15_000,
        per_host_max_concurrent: 1,
        only_main_content_default: true,
        http_retry_threshold_bytes: 0,
    })
    .await;
}

/// A warm entry is not served to a crawl, and the crawl does not disturb it.
#[tokio::test]
async fn a_crawl_does_not_read_the_page_cache() {
    let r = renderer();
    let (server, url) = origin("/crawl-reads").await;

    scrape(&r, &url).await;
    assert_eq!(hits(&server, "/crawl-reads").await, 1);
    assert!(scrape(&r, &url).await.cached, "fixture: the entry is warm");

    crawl(&r, &url).await;
    assert_eq!(
        hits(&server, "/crawl-reads").await,
        2,
        "the crawl must fetch live"
    );
    assert!(
        scrape(&r, &url).await.cached,
        "the entry survives the crawl"
    );
    assert_eq!(hits(&server, "/crawl-reads").await, 2);
}

/// A crawled page leaves nothing behind for a later single scrape.
#[tokio::test]
async fn a_crawl_does_not_write_the_page_cache() {
    let r = renderer();
    let (server, url) = origin("/crawl-writes").await;

    crawl(&r, &url).await;
    assert_eq!(hits(&server, "/crawl-writes").await, 1);

    let data = scrape(&r, &url).await;
    assert!(!data.cached, "a crawl seeded the cache");
    assert_eq!(hits(&server, "/crawl-writes").await, 2);
}
