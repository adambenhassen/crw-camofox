//! The page cache stays out of two surfaces: MCP `crw_scrape` and search
//! enrichment. Neither reads a warm entry, and neither leaves one behind for a
//! REST scrape.
//!
//! Own test binary because it sets `CRW_ALLOW_LOOPBACK_FOR_TESTS`, which is
//! process-global. The cache is process-global too, so every test uses a URL no
//! other test does.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum_test::TestServer;
use crw_core::Deadline;
use crw_core::config::{AppConfig, RendererMode};
use crw_core::error::CrwResult;
use crw_core::types::FetchResult;
use crw_renderer::FallbackRenderer;
use crw_renderer::traits::PageFetcher;
use crw_server::app::create_app;
use crw_server::state::AppState;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn allow_loopback() {
    // SAFETY: test-only env opt-in, set before the server handles any request.
    unsafe {
        std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1");
    }
}

fn article() -> String {
    let para = "Lume Roasters sources single origin coffee from smallholder farms. ";
    format!(
        "<html><head><title>Lume</title></head><body><main><h1>Coffee</h1><p>{}</p></main></body></html>",
        para.repeat(40)
    )
}

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
        Ok(FetchResult {
            url: url.to_string(),
            final_url: None,
            status_code: 200,
            html: article(),
            content_type: Some("text/html".to_string()),
            raw_bytes: None,
            rendered_with: Some("http".to_string()),
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
        "http"
    }

    fn supports_js(&self) -> bool {
        false
    }

    async fn is_available(&self) -> bool {
        true
    }
}

fn app_with_counting_tier() -> (TestServer, Arc<AtomicUsize>) {
    allow_loopback();
    let mut cfg = AppConfig::default();
    cfg.renderer.mode = RendererMode::None;
    let mut state = AppState::new(cfg).expect("AppState");
    let calls = Arc::new(AtomicUsize::new(0));
    state.renderer = Arc::new(
        FallbackRenderer::new(
            &state.config.renderer,
            &state.config.crawler.user_agent,
            None,
            &state.config.crawler.stealth,
        )
        .expect("renderer")
        .with_fetchers(Arc::new(Http(calls.clone())), vec![]),
    );
    (TestServer::new(create_app(state)), calls)
}

async fn mcp_scrape(server: &TestServer, url: &str) -> Value {
    let resp = server
        .post("/mcp")
        .content_type("application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "crw_scrape", "arguments": { "url": url } }
        }))
        .await;
    resp.assert_status_ok();
    resp.json()
}

async fn rest_scrape(server: &TestServer, url: &str) -> Value {
    server
        .post("/v1/scrape")
        .json(&json!({ "url": url }))
        .await
        .json()
}

fn n(calls: &Arc<AtomicUsize>) -> usize {
    calls.load(Ordering::SeqCst)
}

#[tokio::test]
async fn mcp_crw_scrape_neither_reads_nor_writes_the_cache() {
    let (server, calls) = app_with_counting_tier();
    let warm = "http://127.0.0.1:41101/warm";
    let cold = "http://127.0.0.1:41101/cold";

    // A REST scrape warms the entry...
    assert_eq!(rest_scrape(&server, warm).await["success"], true);
    assert_eq!(n(&calls), 1);

    // ...MCP does not read it, twice over...
    for expected in [2, 3] {
        let reply = mcp_scrape(&server, warm).await;
        assert!(reply["error"].is_null(), "{reply}");
        assert_eq!(n(&calls), expected, "MCP crw_scrape was served from cache");
    }
    // ...and did not disturb it.
    let again = rest_scrape(&server, warm).await;
    assert_eq!(again["data"]["cached"], true, "{again}");
    assert_eq!(n(&calls), 3);

    // Nor does an MCP fetch leave an entry behind for a REST caller.
    mcp_scrape(&server, cold).await;
    assert_eq!(n(&calls), 4);
    let rest = rest_scrape(&server, cold).await;
    assert!(rest["data"].get("cached").is_none(), "{rest}");
    assert_eq!(n(&calls), 5, "an MCP fetch seeded the cache");
}

/// Camofox search stub returning one result that points back at the page, plus
/// the page itself.
async fn search_origin() -> (MockServer, String) {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/enriched"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(article())
                .insert_header("content-type", "text/html"),
        )
        .mount(&mock)
        .await;
    let page_url = format!("{}/enriched", mock.uri());
    let rows = serde_json::to_string(&json!([
        { "url": page_url, "title": "page", "content": "snippet" }
    ]))
    .unwrap();
    for (p, body) in [
        ("/tabs", json!({ "tabId": "t1" })),
        ("/tabs/t1/navigate", json!({ "ok": true })),
        ("/tabs/t1/wait", json!({ "ok": true })),
        ("/tabs/t1/evaluate", json!({ "result": rows })),
    ] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&mock)
            .await;
    }
    (mock, page_url)
}

async fn page_hits(mock: &MockServer) -> usize {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/enriched")
        .count()
}

#[tokio::test]
async fn search_enrichment_neither_reads_nor_writes_the_cache() {
    allow_loopback();
    let (mock, page_url) = search_origin().await;
    let config: AppConfig = toml::from_str(&format!(
        r#"
[search]
enabled = true
timeout_ms = 5000

[renderer]
mode = "none"

[renderer.camofox]
base_url = "{}"
"#,
        mock.uri()
    ))
    .unwrap();
    let server = TestServer::new(create_app(AppState::new(config).expect("AppState")));

    let search = |query: &'static str| {
        server.post("/v1/search").json(&json!({
            "query": query,
            "limit": 1,
            "scrapeOptions": { "formats": ["markdown"], "timeout": 10_000 }
        }))
    };

    // A REST scrape warms the entry for the page...
    assert_eq!(rest_scrape(&server, &page_url).await["success"], true);
    assert_eq!(page_hits(&mock).await, 1);

    // ...enrichment does not read it (a different query so the search leg itself
    // is not replayed from the search cache)...
    let body: Value = search("enrich-one").await.json();
    assert!(
        body["data"]["results"][0]["markdown"]
            .as_str()
            .is_some_and(|m| m.contains("Lume")),
        "enrichment must still return the page: {body}"
    );
    assert_eq!(
        page_hits(&mock).await,
        2,
        "enrichment was served from cache"
    );

    // ...and a second enrichment fetches again instead of replaying its own.
    search("enrich-two").await;
    assert_eq!(page_hits(&mock).await, 3);

    // The REST entry survived untouched.
    let again = rest_scrape(&server, &page_url).await;
    assert_eq!(again["data"]["cached"], true, "{again}");
    assert_eq!(page_hits(&mock).await, 3);
}
