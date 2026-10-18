//! A warm cache entry never turns a rejected URL into a served page.
//!
//! The routes validate the URL before `scrape_url` ever consults the cache. This
//! binary does NOT allow loopback (no `CRW_ALLOW_LOOPBACK_FOR_TESTS`), so the
//! real guard rejects the private target; it warms the exact key the route
//! would build first, so a regression that consulted the cache first would
//! answer 200 here.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum_test::TestServer;
use crw_core::Deadline;
use crw_core::config::{AppConfig, RendererMode};
use crw_core::error::CrwResult;
use crw_core::types::{FetchResult, OutputFormat, ScrapeRequest};
use crw_crawl::single::scrape_url;
use crw_renderer::FallbackRenderer;
use crw_renderer::traits::PageFetcher;
use crw_server::app::create_app;
use crw_server::state::AppState;
use serde_json::json;

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
    let mut cfg = AppConfig::default();
    cfg.renderer.mode = RendererMode::None;
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

/// A warm entry must not turn a rejected URL into a served page. The route
/// validates before `scrape_url` ever consults the cache, so this warms the
/// exact key the route would build and shows the route still refuses.
#[tokio::test]
async fn a_warm_entry_never_bypasses_url_validation() {
    let (state, calls) = state();
    let private = "http://127.0.0.1:9/secret";

    let req = ScrapeRequest {
        url: private.to_string(),
        formats: vec![OutputFormat::Markdown],
        ..Default::default()
    };
    let deadline = || Deadline::from_request_ms(30_000);
    let scrape = |req: ScrapeRequest| {
        let state = state.clone();
        async move {
            scrape_url(
                &req,
                &state.renderer,
                None,
                &state.config.extraction,
                &state.config.crawler.user_agent,
                state.config.crawler.stealth.enabled && state.config.crawler.stealth.inject_headers,
                state.config.renderer.render_js_default,
                deadline(),
            )
            .await
            .expect("scrape")
        }
    };
    assert!(!scrape(req.clone()).await.cached);
    assert!(
        scrape(req).await.cached,
        "fixture: the private URL's entry is warm"
    );
    assert_eq!(n(&calls), 1);

    let server = TestServer::new(create_app(state));
    for route in ["/v1/scrape", "/v2/scrape"] {
        let resp = server.post(route).json(&json!({ "url": private })).await;
        let (status, body) = (resp.status_code().as_u16(), resp.text());
        assert_eq!(
            status, 400,
            "{route} served a private URL from cache: {body}"
        );
    }
    assert_eq!(n(&calls), 1, "validation must precede the fetch layer");
}

fn n(calls: &Arc<AtomicUsize>) -> usize {
    calls.load(Ordering::SeqCst)
}
