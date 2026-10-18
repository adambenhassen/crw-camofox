//! The page cache as seen through the HTTP surfaces: `cached` on `/v1`,
//! `cacheState` on `/v2`, `maxAge` / `storeInCache` on both, batch, and the
//! and batch.
//!
//! The renderer is a counting mock, so the assertion is how often the fetch
//! tier was reached. Own test binary because it sets
//! `CRW_ALLOW_LOOPBACK_FOR_TESTS`, which is process-global (the URLs are
//! loopback literals so no DNS is needed). The cache is process-global too, so
//! every test uses a port no other test does.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum_test::TestServer;
use crw_core::Deadline;
use crw_core::config::{AppConfig, RendererMode};
use crw_core::error::CrwResult;
use crw_core::types::{CrawlState, CrawlStatus, FetchResult, ScrapeRequest};
use crw_renderer::FallbackRenderer;
use crw_renderer::traits::PageFetcher;
use crw_server::app::create_app;
use crw_server::state::AppState;
use serde_json::{Value, json};

struct Http(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl PageFetcher for Http {
    async fn fetch(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        _wait_for_ms: Option<u64>,
        _deadline: Deadline,
    ) -> CrwResult<FetchResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let para = "Lume Roasters sources single origin coffee from smallholder farms. ";
        Ok(FetchResult {
            url: url.to_string(),
            final_url: None,
            status_code: 200,
            html: format!(
                "<html><head><title>Lume</title></head><body><main><h1>Coffee</h1><p>{}</p></main></body></html>",
                para.repeat(40)
            ),
            content_type: Some("text/html".to_string()),
            raw_bytes: None,
            rendered_with: Some("http".to_string()),
            elapsed_ms: 1_500,
            warning: None,
            render_decision: None,
            credit_cost: 0,
            warnings: Vec::new(),
            wall: None,
            truncated: false,
            deadline_exceeded: false,
            captured_responses: Vec::new(),
        })
    }

    fn name(&self) -> &str {
        "http"
    }

    fn supports_js(&self) -> bool {
        false
    }

    async fn is_available(&self) -> bool {
        true
    }
}

fn state() -> (AppState, Arc<AtomicUsize>) {
    // SAFETY: one binary per tests/*.rs, set before any request is handled.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") };
    let mut cfg = AppConfig::default();
    cfg.renderer.mode = RendererMode::None;
    cfg.crawler.respect_robots_txt = false;
    let mut state = AppState::new(cfg).expect("AppState");
    let calls = Arc::new(AtomicUsize::new(0));
    let renderer = FallbackRenderer::new(
        &state.config.renderer,
        &state.config.crawler.user_agent,
        None,
        &state.config.crawler.stealth,
    )
    .expect("renderer")
    .with_fetchers(Arc::new(Http(calls.clone())), vec![]);
    state.renderer = Arc::new(renderer);
    (state, calls)
}

fn n(calls: &Arc<AtomicUsize>) -> usize {
    calls.load(Ordering::SeqCst)
}

async fn post(server: &TestServer, route: &str, body: Value) -> (u16, Value) {
    let resp = server.post(route).json(&body).await;
    (resp.status_code().as_u16(), resp.json())
}

#[tokio::test]
async fn v1_marks_a_replayed_page_cached() {
    let (state, calls) = state();
    let server = TestServer::new(create_app(state));
    let body = json!({ "url": "http://127.0.0.1:41001/page" });

    let (status, first) = post(&server, "/v1/scrape", body.clone()).await;
    assert_eq!(status, 200, "{first}");
    assert!(
        first["data"].get("cached").is_none(),
        "a live fetch must not carry the flag: {first}"
    );

    let (_, second) = post(&server, "/v1/scrape", body.clone()).await;
    assert_eq!(second["data"]["cached"], true, "{second}");
    assert_eq!(n(&calls), 1);
    assert_eq!(first["data"]["markdown"], second["data"]["markdown"]);

    let (_, live) = post(
        &server,
        "/v1/scrape",
        json!({ "url": "http://127.0.0.1:41001/page", "maxAge": 0 }),
    )
    .await;
    assert!(live["data"].get("cached").is_none(), "{live}");
    assert_eq!(n(&calls), 2);
}

#[tokio::test]
async fn v2_reports_cache_state_and_honours_max_age_and_store_in_cache() {
    let (state, calls) = state();
    let server = TestServer::new(create_app(state));
    let url = "http://127.0.0.1:41002/page";

    let (status, miss) = post(&server, "/v2/scrape", json!({ "url": url })).await;
    assert_eq!(status, 200, "{miss}");
    assert_eq!(miss["data"]["metadata"]["cacheState"], "miss", "{miss}");
    let credits = miss["data"]["metadata"]["creditsUsed"].clone();
    assert!(
        credits.as_u64().unwrap_or(0) > 0,
        "fixture must bill: {miss}"
    );

    let (_, hit) = post(&server, "/v2/scrape", json!({ "url": url })).await;
    assert_eq!(hit["data"]["metadata"]["cacheState"], "hit", "{hit}");
    assert_eq!(
        hit["data"]["metadata"]["creditsUsed"], credits,
        "a hit bills like the fetch it replays: {hit}"
    );
    assert_eq!(n(&calls), 1);

    // The SDK sends these as numbers, but a string must not turn into a 400.
    for max_age in [json!(0), json!("0"), json!(0.0)] {
        let (status, live) = post(
            &server,
            "/v2/scrape",
            json!({ "url": url, "maxAge": max_age }),
        )
        .await;
        assert_eq!(status, 200, "{live}");
        assert_eq!(live["data"]["metadata"]["cacheState"], "miss", "{live}");
    }
    assert_eq!(n(&calls), 4, "maxAge 0 always fetches");

    // storeInCache:false reads, but writes nothing.
    let fresh = "http://127.0.0.1:41002/no-store";
    let (_, a) = post(
        &server,
        "/v2/scrape",
        json!({ "url": fresh, "storeInCache": false }),
    )
    .await;
    assert_eq!(a["data"]["metadata"]["cacheState"], "miss");
    let (_, b) = post(&server, "/v2/scrape", json!({ "url": fresh })).await;
    assert_eq!(b["data"]["metadata"]["cacheState"], "miss", "{b}");
}

async fn settled(state: &AppState, id: uuid::Uuid) -> CrawlState {
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let jobs = state.crawl_jobs.read().await;
        let s = jobs.get(&id).expect("job").rx.borrow().clone();
        if !matches!(s.status, CrawlStatus::InProgress) {
            return s;
        }
    }
    panic!("job did not settle");
}

/// Batch scrapes through `scrape_url` with the request's template, so it shares
/// the cache like a single scrape and honours the template's `maxAge`.
#[tokio::test]
async fn batch_shares_the_cache_and_honours_the_templates_max_age() {
    let (state, calls) = state();
    let urls = vec!["http://127.0.0.1:41003/a".to_string()];

    let id = state
        .start_batch_job(urls.clone(), ScrapeRequest::default())
        .await;
    let first = settled(&state, id).await;
    assert!(!first.data[0].cached);
    assert_eq!(n(&calls), 1);

    let id = state
        .start_batch_job(urls.clone(), ScrapeRequest::default())
        .await;
    let second = settled(&state, id).await;
    assert!(second.data[0].cached, "a repeat batch URL should hit");
    assert_eq!(n(&calls), 1);

    let id = state
        .start_batch_job(
            urls,
            ScrapeRequest {
                max_age: Some(0),
                ..Default::default()
            },
        )
        .await;
    let live = settled(&state, id).await;
    assert!(!live.data[0].cached);
    assert_eq!(n(&calls), 2);
}
