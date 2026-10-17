//! A generic anti-bot wall that no renderer tier can clear is a verdict about
//! the target, not a gateway failure. It used to end the ladder as
//! `CrwError::HttpError`, which the server maps to 502 and SDKs retry; it must
//! answer HTTP 200 `success:false` + `anti_bot` on `/v1/scrape`, and fail with
//! no body on `/v2/scrape`.
//!
//! The real HTTP tier talks to a mock origin; only the browser tiers are stubbed
//! (no browser exists in CI), so the whole chain from the ladder tail to the
//! route envelope runs for real.
//!
//! Own test binary because it sets `CRW_ALLOW_LOOPBACK_FOR_TESTS`, which is
//! process-global.

use std::collections::HashMap;
use std::sync::Arc;

use axum_test::TestServer;
use crw_core::Deadline;
use crw_core::config::{AppConfig, RendererMode};
use crw_core::error::CrwResult;
use crw_core::types::{CrawlRequest, CrawlState, CrawlStatus, FetchResult, ScrapeRequest};
use crw_renderer::FallbackRenderer;
use crw_renderer::http_only::HttpFetcher;
use crw_renderer::traits::PageFetcher;
use crw_server::app::create_app;
use crw_server::state::AppState;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const WALL: &str =
    "<html><body><p>Checking your browser before accessing the site</p></body></html>";

/// A browser tier that answers every URL with the same page.
struct StaticTier {
    name: &'static str,
    html: String,
}

#[async_trait::async_trait]
impl PageFetcher for StaticTier {
    async fn fetch(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        _wait_for_ms: Option<u64>,
        _deadline: Deadline,
    ) -> CrwResult<FetchResult> {
        Ok(FetchResult {
            url: url.to_string(),
            final_url: None,
            status_code: 200,
            html: self.html.clone(),
            content_type: Some("text/html".to_string()),
            raw_bytes: None,
            rendered_with: Some(self.name.to_string()),
            elapsed_ms: 0,
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
        self.name
    }

    fn supports_js(&self) -> bool {
        true
    }

    async fn is_available(&self) -> bool {
        true
    }
}

async fn origin() -> MockServer {
    let server = MockServer::start().await;
    let article = format!(
        "<html><head><title>Article</title></head><body><article><h1>Article</h1>{}</article></body></html>",
        "<p>A real paragraph of article prose that a reader came here for.</p>".repeat(80)
    );
    let page = |status: u16, body: &str| {
        ResponseTemplate::new(status)
            .insert_header("content-type", "text/html")
            .set_body_string(body.to_string())
    };
    for (p, resp) in [
        // The wall is what the origin serves to us, and what every browser tier
        // meets too.
        ("/wall", page(200, WALL)),
        // A 403 that still serves the real page: the walled ladder must not
        // displace it.
        ("/forbidden-article", page(403, &article)),
    ] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(resp)
            .mount(&server)
            .await;
    }
    server
}

fn app() -> TestServer {
    TestServer::new(create_app(walled_state()))
}

fn walled_state() -> AppState {
    // SAFETY: one binary per tests/*.rs, set before any fetcher is built.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") };
    let mut cfg = AppConfig::default();
    cfg.renderer.mode = RendererMode::None;
    cfg.crawler.respect_robots_txt = false;
    let mut state = AppState::new(cfg).expect("AppState");
    let tier = |name| {
        Arc::new(StaticTier {
            name,
            html: WALL.to_string(),
        }) as Arc<dyn PageFetcher>
    };
    // The real HTTP tier, so the origin is reached over the wire; two browser
    // tiers that can only ever meet the wall.
    let renderer = FallbackRenderer::new(
        &state.config.renderer,
        &state.config.crawler.user_agent,
        None,
        &state.config.crawler.stealth,
    )
    .expect("renderer")
    .with_fetchers(
        Arc::new(HttpFetcher::new(
            &state.config.crawler.user_agent,
            None,
            false,
        )),
        vec![tier("lightpanda"), tier("camofox")],
    );
    state.renderer = Arc::new(renderer);
    state
}

async fn settled(state: &AppState, id: uuid::Uuid) -> CrawlState {
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let jobs = state.crawl_jobs.read().await;
        let s = jobs.get(&id).expect("job").rx.borrow().clone();
        if !matches!(s.status, CrawlStatus::InProgress) {
            return s;
        }
    }
    panic!("job did not settle");
}

async fn post(server: &TestServer, route: &str, url: &str) -> (u16, Value) {
    let resp = server.post(route).json(&json!({ "url": url })).await;
    (resp.status_code().as_u16(), resp.json())
}

#[tokio::test]
async fn v1_scrape_answers_200_anti_bot_for_a_wall_no_tier_can_clear() {
    let origin = origin().await;
    let server = app();
    let (status, body) = post(&server, "/v1/scrape", &format!("{}/wall", origin.uri())).await;

    assert_eq!(status, 200, "a refused target is not a bad gateway: {body}");
    assert_eq!(body["success"], false, "{body}");
    assert_eq!(body["error_code"], "anti_bot", "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("generic_block")),
        "the verdict must be named: {body}"
    );
    assert!(
        body["data"]["markdown"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the interstitial must not ship as content: {body}"
    );
}

#[tokio::test]
async fn v2_scrape_fails_a_wall_no_tier_can_clear_without_a_502() {
    let origin = origin().await;
    let server = app();
    let (status, body) = post(&server, "/v2/scrape", &format!("{}/wall", origin.uri())).await;

    assert_eq!(status, 200, "a refused target is not a bad gateway: {body}");
    assert_eq!(body["success"], false, "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("anti-bot")),
        "v2 must say why: {body}"
    );
    assert_eq!(
        body["data"]["metadata"]["creditsUsed"], 0,
        "a wall is never billed: {body}"
    );
    assert!(
        body["data"]["markdown"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the interstitial must not ship as content: {body}"
    );
}

/// The recall half: a 403 that still serves the real page ships as the HTTP
/// error page it is, never as an `anti_bot` verdict from the walled ladder.
#[tokio::test]
async fn a_walled_ladder_does_not_displace_a_403_page_with_real_content() {
    let origin = origin().await;
    let server = app();
    let (status, body) = post(
        &server,
        "/v1/scrape",
        &format!("{}/forbidden-article", origin.uri()),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_ne!(body["error_code"], "anti_bot", "{body}");
    assert!(
        body["data"]["markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("real paragraph of article prose"),
        "the real page must survive: {body}"
    );
}

/// Batch and crawl take the same verdict through `scrape_url` / the renderer:
/// the wall is counted `blocked` (so `completed - blocked` leaves it unbilled)
/// and its shell is cleared.
#[tokio::test]
async fn batch_counts_a_wall_no_tier_can_clear_as_blocked() {
    let origin = origin().await;
    let state = walled_state();
    let url = format!("{}/wall", origin.uri());
    let id = state
        .start_batch_job(vec![url], ScrapeRequest::default())
        .await;
    let s = settled(&state, id).await;

    assert_eq!((s.completed, s.blocked), (1, 1), "{s:?}");
    let page = &s.data[0];
    assert!(page.block.as_ref().is_some_and(|b| b.is_wall()), "{page:?}");
    assert!(page.markdown.is_none(), "the shell must be cleared");
}

#[tokio::test]
async fn crawl_counts_a_wall_no_tier_can_clear_as_blocked() {
    let origin = origin().await;
    let state = walled_state();
    let req: CrawlRequest =
        serde_json::from_value(json!({ "url": format!("{}/wall", origin.uri()) }))
            .expect("crawl request");
    let id = state.start_crawl_job(req).await;
    let s = settled(&state, id).await;

    assert_eq!((s.completed, s.blocked), (1, 1), "{s:?}");
    assert!(s.data[0].block.as_ref().is_some_and(|b| b.is_wall()));
    assert!(s.data[0].markdown.is_none());
}
