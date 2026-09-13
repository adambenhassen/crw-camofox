//! `scrape_url` driven through the real renderer ladder with mock tiers: the
//! post-extract escalation (a thin LightPanda render escalates to camofox, and
//! each gate that stops or fails it is visible to the caller) and non-HTML
//! bodies end to end.

use std::collections::HashMap;
use std::sync::Arc;

use crw_core::Deadline;
use crw_core::config::{ExtractionConfig, RendererConfig, RendererMode, StealthConfig};
use crw_core::error::{CrwError, CrwResult};
use crw_core::types::{FetchResult, ScrapeRequest};
use crw_crawl::single::scrape_url;
use crw_renderer::FallbackRenderer;
use crw_renderer::traits::PageFetcher;

struct Tier {
    name: &'static str,
    body: Result<String, String>,
    content_type: &'static str,
    truncated: bool,
}

#[async_trait::async_trait]
impl PageFetcher for Tier {
    async fn fetch(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        _wait_for_ms: Option<u64>,
        _deadline: Deadline,
    ) -> CrwResult<FetchResult> {
        let html = self.body.clone().map_err(CrwError::RendererError)?;
        Ok(FetchResult {
            url: url.to_string(),
            final_url: None,
            status_code: 200,
            html,
            content_type: Some(self.content_type.to_string()),
            raw_bytes: None,
            rendered_with: Some(self.name.to_string()),
            elapsed_ms: 0,
            warning: None,
            render_decision: None,
            credit_cost: 0,
            warnings: Vec::new(),
            wall: None,
            truncated: self.truncated,
            deadline_exceeded: false,
            captured_responses: Vec::new(),
        })
    }

    fn name(&self) -> &str {
        self.name
    }

    fn supports_js(&self) -> bool {
        self.name != "http"
    }

    async fn is_available(&self) -> bool {
        true
    }
}

fn tier(name: &'static str, body: Result<String, String>) -> Arc<dyn PageFetcher> {
    Arc::new(Tier {
        name,
        body,
        content_type: "text/html",
        truncated: false,
    })
}

/// A React shell: the HTTP tier's body sends auto mode to the JS ladder.
fn spa_shell() -> String {
    r#"<html><head><script src="/static/js/main.js"></script></head><body><div id="root"></div></body></html>"#
        .to_string()
}

/// Enough text for the ladder to accept, far too little markdown to keep.
fn thin_render() -> String {
    "<html><body><main><p>Loading the latest listings for you, please wait a moment.</p></main></body></html>"
        .to_string()
}

fn article(paragraphs: usize) -> String {
    let p = "<p>Camofox rendered this paragraph of the real article body, with enough \
             words in it to read as prose rather than navigation chrome.</p>";
    format!(
        "<html><body><article><h1>Real article</h1>{}</article></body></html>",
        p.repeat(paragraphs)
    )
}

fn renderer(js: Vec<Arc<dyn PageFetcher>>) -> Arc<FallbackRenderer> {
    renderer_with_http(tier("http", Ok(spa_shell())), js)
}

fn renderer_with_http(
    http: Arc<dyn PageFetcher>,
    js: Vec<Arc<dyn PageFetcher>>,
) -> Arc<FallbackRenderer> {
    let cfg = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    Arc::new(
        FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default())
            .expect("renderer")
            .with_fetchers(http, js),
    )
}

async fn scrape(r: &Arc<FallbackRenderer>) -> CrwResult<crw_core::types::ScrapeData> {
    let req: ScrapeRequest =
        serde_json::from_value(serde_json::json!({ "url": "https://spa.example/listings" }))
            .expect("request");
    scrape_url(
        &req,
        r,
        None,
        &ExtractionConfig::default(),
        "crw-test",
        false,
        None,
        Deadline::from_request_ms(30_000),
    )
    .await
}

#[tokio::test]
async fn thin_lightpanda_render_escalates_to_camofox() {
    let r = renderer(vec![
        tier("lightpanda", Ok(thin_render())),
        tier("camofox", Ok(article(30))),
    ]);
    let data = scrape(&r).await.expect("scrape");
    assert_eq!(data.metadata.rendered_with.as_deref(), Some("camofox"));
    assert!(
        data.markdown
            .as_deref()
            .unwrap_or_default()
            .contains("Real article"),
        "{:?}",
        data.markdown
    );
}

/// Shorter than the 2000-byte LightPanda retry threshold, but more than the
/// thin render: camofox is the top tier, so its page wins.
#[tokio::test]
async fn camofox_page_below_the_retry_threshold_still_replaces_a_thinner_one() {
    let r = renderer(vec![
        tier("lightpanda", Ok(thin_render())),
        tier("camofox", Ok(article(3))),
    ]);
    let data = scrape(&r).await.expect("scrape");
    assert_eq!(data.metadata.rendered_with.as_deref(), Some("camofox"));
}

#[tokio::test]
async fn no_tier_above_lightpanda_skips_with_a_warning() {
    let r = renderer(vec![tier("lightpanda", Ok(thin_render()))]);
    let data = scrape(&r).await.expect("scrape");
    assert_eq!(data.metadata.rendered_with.as_deref(), Some("lightpanda"));
    let warning = data.warning.unwrap_or_default();
    assert!(warning.contains("JS escalation skipped"), "{warning}");
}

#[tokio::test]
async fn failed_camofox_escalation_is_tagged_on_the_response() {
    let r = renderer(vec![
        tier("lightpanda", Ok(thin_render())),
        tier("camofox", Err("navigation failed".to_string())),
    ]);
    let data = scrape(&r).await.expect("the thin page still ships");
    assert_eq!(data.metadata.rendered_with.as_deref(), Some("lightpanda"));
    let warning = data.warning.unwrap_or_default();
    assert!(
        warning.contains(crw_renderer::JS_ESCALATION_FAILED),
        "{warning}"
    );
}

/// A plain-text body (a `requirements.txt`) has no `<body>`, which the HTML
/// structural checks read as a broken page. It is content: it must ship as
/// such, not be escalated into a browser or failed as `structural_failure`.
#[tokio::test]
async fn plain_text_body_ships_as_content() {
    let text = "tokio==1.40\nserde==1.0\nreqwest==0.12\n";
    let http = Arc::new(Tier {
        name: "http",
        body: Ok(text.to_string()),
        content_type: "text/plain",
        truncated: false,
    }) as Arc<dyn PageFetcher>;
    let r = renderer_with_http(
        http,
        vec![tier("camofox", Err("must not be reached".into()))],
    );
    let data = scrape(&r).await.expect("scrape");
    assert!(data.block.is_none(), "{:?}", data.block);
    assert!(data.http_error().is_none());
    assert_eq!(data.metadata.rendered_with.as_deref(), Some("http"));
    assert!(
        data.markdown
            .as_deref()
            .unwrap_or_default()
            .contains("tokio==1.40"),
        "{:?}",
        data.markdown
    );
}

/// Item 15: a render whose budget expired with nothing extracted fails as a
/// timeout reporting the caller's budget, not the time spent.
#[tokio::test]
async fn empty_truncated_render_fails_with_the_requested_budget() {
    let lightpanda = Arc::new(Tier {
        name: "lightpanda",
        body: Ok(String::new()),
        content_type: "text/html",
        truncated: true,
    }) as Arc<dyn PageFetcher>;
    let r = renderer(vec![lightpanda]);
    match scrape(&r).await {
        Err(CrwError::Timeout(ms)) => assert_eq!(ms, 30_000, "the requested budget"),
        other => panic!("expected Timeout(30000), got {other:?}"),
    }
}

fn tier_truncated(name: &'static str, body: String, truncated: bool) -> Arc<dyn PageFetcher> {
    Arc::new(Tier {
        name,
        body: Ok(body),
        content_type: "text/html",
        truncated,
    })
}

/// `truncated` describes the body, so it follows the render the post-extract
/// escalation keeps. A short real article (not a placeholder) passes the ladder
/// and only the markdown threshold escalates it.
#[tokio::test]
async fn accepted_escalation_reports_the_kept_renders_truncation() {
    let r = renderer(vec![
        tier_truncated("lightpanda", article(2), true),
        tier_truncated("camofox", article(30), false),
    ]);
    let data = scrape(&r).await.expect("scrape");
    assert_eq!(data.metadata.rendered_with.as_deref(), Some("camofox"));
    assert!(!data.truncated, "camofox recovered the full page");

    let r = renderer(vec![
        tier_truncated("lightpanda", article(2), false),
        tier_truncated("camofox", article(30), true),
    ]);
    let data = scrape(&r).await.expect("scrape");
    assert_eq!(data.metadata.rendered_with.as_deref(), Some("camofox"));
    assert!(data.truncated, "the kept camofox render was cut off");
}
