//! `scrape_url` against the process-global page cache, driven through the real
//! renderer ladder with counting mock tiers.
//!
//! The assertion that matters in every one of these is how many times a TIER was
//! reached. A unit test can prove the cache stores and loads; only counting real
//! fetches proves the fetch was actually skipped, and that a bypass really did go
//! back to the network.
//!
//! The cache is process-global and these tests run in parallel in one process,
//! so every test works on a URL no other test can produce: the host carries the
//! test name.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crw_core::Deadline;
use crw_core::config::{ExtractionConfig, RendererConfig, RendererMode, StealthConfig};
use crw_core::error::{CrwError, CrwResult};
use crw_core::types::{
    BlockOutcome, ChangeTrackingMode, ChangeTrackingOptions, ChangeTrackingSnapshot, FetchResult,
    OutputFormat, RequestedRenderer, ScrapeData, ScrapeRequest,
};
use crw_crawl::single::scrape_url;
use crw_renderer::FallbackRenderer;
use crw_renderer::traits::PageFetcher;

const UA: &str = "crw-test";

/// Long enough to sit in the top budget bucket, so a bucket mismatch never
/// explains a miss in these tests.
const DEADLINE_MS: u64 = 60_000;

/// A mock tier that counts how often it is reached.
struct Tier {
    name: &'static str,
    body: Result<String, String>,
    status: u16,
    wall: Option<BlockOutcome>,
    truncated: bool,
    /// A document body: served as `application/pdf` with no HTML.
    pdf: Option<Vec<u8>>,
    calls: Arc<AtomicUsize>,
}

impl Tier {
    fn new(name: &'static str, body: Result<String, String>) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name,
                body,
                status: 200,
                wall: None,
                truncated: false,
                pdf: None,
                calls: calls.clone(),
            },
            calls,
        )
    }
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let html = self.body.clone().map_err(CrwError::RendererError)?;
        Ok(FetchResult {
            url: url.to_string(),
            final_url: None,
            status_code: self.status,
            html,
            content_type: Some(
                if self.pdf.is_some() {
                    "application/pdf"
                } else {
                    "text/html"
                }
                .to_string(),
            ),
            raw_bytes: self.pdf.clone(),
            rendered_with: Some(self.name.to_string()),
            elapsed_ms: 2_000,
            warning: None,
            render_decision: None,
            credit_cost: 0,
            warnings: Vec::new(),
            wall: self.wall.clone(),
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

fn renderer_of(http: Tier, js: Vec<Tier>) -> Arc<FallbackRenderer> {
    let cfg = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    Arc::new(
        FallbackRenderer::new(&cfg, UA, None, &StealthConfig::default())
            .expect("renderer")
            .with_fetchers(
                Arc::new(http),
                js.into_iter()
                    .map(|t| Arc::new(t) as Arc<dyn PageFetcher>)
                    .collect(),
            ),
    )
}

/// Enough text that markdown extraction is never byte-thin for any tier (the
/// LightPanda bar is 2000 bytes), which would otherwise keep it out of the cache
/// as a suspected shell, and past the error-page size bar.
fn article() -> String {
    let para = "Lume Roasters sources single origin coffee from smallholder farms. ";
    format!(
        "<html><head><title>Lume</title></head><body><main><h1>Our coffee</h1><p>{}</p></main></body></html>",
        para.repeat(40)
    )
}

/// A React shell: the HTTP tier's body sends auto mode to the JS ladder.
fn spa_shell() -> String {
    r#"<html><head><script src="/static/js/main.js"></script></head><body><div id="root"></div></body></html>"#
        .to_string()
}

/// Enough text that the renderer ladder accepts the LightPanda render, far too
/// little markdown (under the 2000 byte LightPanda bar) to keep: `scrape_url`
/// itself, not the ladder, then escalates to the next tier.
fn thin_render() -> String {
    format!(
        "<html><body><main><p>{}</p></main></body></html>",
        "Loading the latest listings for you, please wait a moment. ".repeat(10)
    )
}

/// A plain HTTP renderer that serves `article()` and no browser tier.
fn plain() -> (Arc<FallbackRenderer>, Arc<AtomicUsize>) {
    let (http, calls) = Tier::new("http", Ok(article()));
    (renderer_of(http, vec![]), calls)
}

fn url(name: &str) -> String {
    format!("https://{name}.cache.test/page")
}

fn req(name: &str) -> ScrapeRequest {
    ScrapeRequest {
        url: url(name),
        formats: vec![OutputFormat::Markdown],
        ..Default::default()
    }
}

async fn try_run(r: &Arc<FallbackRenderer>, req: &ScrapeRequest) -> CrwResult<ScrapeData> {
    scrape_url(
        req,
        r,
        None,
        &ExtractionConfig::default(),
        UA,
        false,
        None,
        Deadline::from_request_ms(DEADLINE_MS),
    )
    .await
}

async fn run(r: &Arc<FallbackRenderer>, req: &ScrapeRequest) -> ScrapeData {
    try_run(r, req).await.expect("scrape")
}

fn n(calls: &Arc<AtomicUsize>) -> usize {
    calls.load(Ordering::SeqCst)
}

/// The headline claim: the same page twice costs one fetch, and the replay is
/// the same page: same content, same renderer, same billing.
#[tokio::test]
async fn a_repeat_scrape_does_not_reach_the_renderer() {
    let (r, calls) = plain();

    let first = run(&r, &req("repeat")).await;
    assert!(!first.cached, "the first scrape cannot be a hit");
    assert_eq!(n(&calls), 1);

    let second = run(&r, &req("repeat")).await;
    assert!(second.cached, "the second scrape should have been a hit");
    assert_eq!(n(&calls), 1, "the renderer was reached twice");
    assert_eq!(second.markdown, first.markdown);
    assert_eq!(second.metadata.rendered_with, first.metadata.rendered_with);
    assert!(first.credit_cost > 0, "fixture must be priced");
    assert_eq!(
        second.credit_cost, first.credit_cost,
        "a hit bills like the fetch it replays"
    );
    assert!(
        second.metadata.elapsed_ms < 2_000,
        "a hit must report its own replay time, not the original fetch's"
    );
}

/// The reason the feature exists. Extraction options are not part of the key,
/// so a second pass asking for different output still shares one fetch.
#[tokio::test]
async fn different_output_options_share_one_fetch() {
    let (r, calls) = plain();

    run(&r, &req("outputs")).await;
    assert_eq!(n(&calls), 1);

    let second = run(
        &r,
        &ScrapeRequest {
            formats: vec![OutputFormat::Markdown, OutputFormat::Html],
            only_main_content: Some(false),
            include_tags: vec!["p".into()],
            ..req("outputs")
        },
    )
    .await;

    assert!(second.cached, "a different output shape refetched the page");
    assert_eq!(n(&calls), 1);
    assert!(second.html.is_some(), "the new format was still produced");
}

/// The documented escape hatch has to actually reach the network.
#[tokio::test]
async fn max_age_zero_always_fetches() {
    let (r, calls) = plain();

    run(&r, &req("zero")).await;
    let second = run(
        &r,
        &ScrapeRequest {
            max_age: Some(0),
            ..req("zero")
        },
    )
    .await;

    assert!(!second.cached);
    assert_eq!(n(&calls), 2);
}

/// `storeInCache: false` is Firecrawl's write switch: this request may read, but
/// must leave nothing behind.
#[tokio::test]
async fn store_in_cache_false_reads_but_writes_nothing() {
    let (r, calls) = plain();
    let no_store = ScrapeRequest {
        store_in_cache: Some(false),
        ..req("nostore")
    };

    let first = run(&r, &no_store).await;
    assert!(!first.cached);
    assert_eq!(n(&calls), 1);

    let second = run(&r, &req("nostore")).await;
    assert!(!second.cached, "a no-store request still seeded the cache");
    assert_eq!(n(&calls), 2);

    // Reading stays allowed: the entry the second request wrote is served.
    let third = run(&r, &no_store).await;
    assert!(third.cached);
    assert_eq!(n(&calls), 2);
}

/// Change tracking diffs a LIVE page against the caller's snapshot. Served from
/// cache it would report "unchanged" for a page that did change, which is a
/// silent wrong answer rather than a visible error.
#[tokio::test]
async fn change_tracking_never_reads_the_cache() {
    let (r, calls) = plain();

    run(&r, &req("tracking")).await;
    assert_eq!(n(&calls), 1);

    let tracked = run(
        &r,
        &ScrapeRequest {
            formats: vec![OutputFormat::Markdown, OutputFormat::ChangeTracking],
            change_tracking: Some(ChangeTrackingOptions {
                modes: vec![ChangeTrackingMode::GitDiff],
                previous: Some(ChangeTrackingSnapshot {
                    markdown: Some("something else entirely".to_string()),
                    content_hash: crw_diff::snapshot::hash_markdown("something else entirely"),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..req("tracking")
        },
    )
    .await;

    assert!(!tracked.cached);
    assert_eq!(n(&calls), 2, "change tracking was answered from cache");
}

/// Caller headers are where a cookie or a token lives. A page fetched with one
/// is personal to that caller and must not sit in a process-global map, nor be
/// readable by anyone else, however the key is shaped.
#[tokio::test]
async fn a_request_with_caller_headers_is_never_cached() {
    let (r, calls) = plain();
    let with_headers = ScrapeRequest {
        headers: [("Cookie".to_string(), "session=alice".to_string())]
            .into_iter()
            .collect(),
        ..req("headers")
    };

    assert!(!run(&r, &with_headers).await.cached);
    let second = run(&r, &with_headers).await;
    assert!(!second.cached, "an authenticated page was replayed");
    assert_eq!(n(&calls), 2);

    // It left nothing behind for a caller who sends no headers...
    let anonymous = run(&r, &req("headers")).await;
    assert!(!anonymous.cached, "a header-bearing fetch seeded the cache");
    assert_eq!(n(&calls), 3);

    // ...and an anonymous entry is not readable by a header-bearing caller.
    let again = run(&r, &with_headers).await;
    assert!(!again.cached, "a header-bearing request read the cache");
    assert_eq!(n(&calls), 4);
}

/// The stealth-override arm draws a fresh random user agent per request, so its
/// fetch is not reproducible from the request alone.
#[tokio::test]
async fn a_stealth_override_is_never_cached() {
    let (r, calls) = plain();
    let overridden = ScrapeRequest {
        stealth: Some(true),
        ..req("stealth")
    };

    run(&r, &overridden).await;
    let second = run(&r, &overridden).await;
    assert!(!second.cached);
    assert_eq!(n(&calls), 2);

    let anonymous = run(&r, &req("stealth")).await;
    assert!(!anonymous.cached, "a stealth override seeded the cache");
}

/// Everything that changes what the renderer is asked for is a different
/// fetch, so it must not share an entry.
#[tokio::test]
async fn render_js_wait_for_and_the_renderer_pin_are_part_of_the_key() {
    let (http, http_calls) = Tier::new("http", Ok(article()));
    let (lp, lp_calls) = Tier::new("lightpanda", Ok(article()));
    let r = renderer_of(http, vec![lp]);

    run(&r, &req("keyed")).await;
    assert_eq!(n(&http_calls), 1);

    for (label, variant) in [
        (
            "renderJs",
            ScrapeRequest {
                render_js: Some(false),
                ..req("keyed")
            },
        ),
        (
            "renderJs true",
            ScrapeRequest {
                render_js: Some(true),
                ..req("keyed")
            },
        ),
        (
            "waitFor",
            ScrapeRequest {
                wait_for: Some(250),
                ..req("keyed")
            },
        ),
        (
            "renderer pin",
            ScrapeRequest {
                renderer: Some(RequestedRenderer::Lightpanda),
                ..req("keyed")
            },
        ),
    ] {
        let before = n(&http_calls) + n(&lp_calls);
        let data = run(&r, &variant).await;
        assert!(!data.cached, "{label} reused the default fetch");
        assert!(
            n(&http_calls) + n(&lp_calls) > before,
            "{label} did not reach a tier"
        );
        let again = run(&r, &variant).await;
        assert!(again.cached, "{label} should hit its own entry");
    }
}

/// Whether a fetch could escalate to a browser is part of the key, because an
/// escalated fetch holds a rendered DOM rather than the HTTP source. A json-only
/// or rawHtml-only request cannot escalate, so it keeps its own entry.
#[tokio::test]
async fn a_request_that_cannot_escalate_keeps_its_own_entry() {
    let (http, http_calls) = Tier::new("http", Ok(article()));
    let (lp, _) = Tier::new("lightpanda", Ok(article()));
    let r = renderer_of(http, vec![lp]);

    run(&r, &req("escalate-key")).await;
    assert_eq!(n(&http_calls), 1);

    let raw = run(
        &r,
        &ScrapeRequest {
            formats: vec![OutputFormat::RawHtml],
            ..req("escalate-key")
        },
    )
    .await;
    assert!(
        !raw.cached,
        "a non-escalating request shared an escalating entry"
    );
    assert_eq!(n(&http_calls), 2);
}

/// A failed fetch is never stored, so a retry always gets a real attempt. This
/// is published behaviour, not an implementation detail.
#[tokio::test]
async fn an_origin_error_page_is_never_cached() {
    let (http, calls) = Tier::new("http", Ok(article()));
    let r = renderer_of(
        Tier {
            status: 500,
            ..http
        },
        vec![],
    );

    let _ = try_run(&r, &req("error500")).await;
    let before = n(&calls);
    let second = try_run(&r, &req("error500")).await;

    assert!(n(&calls) > before, "a failed fetch was replayed from cache");
    assert!(second.map(|d| !d.cached).unwrap_or(true));
}

/// A renderer error is an `Err`, and an `Err` is never stored.
#[tokio::test]
async fn a_renderer_error_is_never_cached() {
    let (http, calls) = Tier::new("http", Err("connection reset".to_string()));
    let r = renderer_of(http, vec![]);

    assert!(try_run(&r, &req("rendererr")).await.is_err());
    assert!(try_run(&r, &req("rendererr")).await.is_err());
    assert_eq!(n(&calls), 2);
}

/// A thin body is treated as an unrendered shell: pinning one would serve it to
/// every later reader for the whole window.
#[tokio::test]
async fn a_thin_page_is_never_cached() {
    let (http, calls) = Tier::new("http", Ok("<html><body><p>hi</p></body></html>".into()));
    let r = renderer_of(http, vec![]);

    run(&r, &req("thin")).await;
    let second = run(&r, &req("thin")).await;
    assert!(!second.cached);
    assert_eq!(n(&calls), 2);
}

/// A partial DOM snapshotted on budget expiry is a 200 with a body, and must not
/// be pinned.
#[tokio::test]
async fn a_truncated_render_is_never_cached() {
    let (http, calls) = Tier::new("http", Ok(article()));
    let r = renderer_of(
        Tier {
            truncated: true,
            ..http
        },
        vec![],
    );

    run(&r, &req("truncated")).await;
    let second = run(&r, &req("truncated")).await;
    assert!(!second.cached);
    assert_eq!(n(&calls), 2);
}

/// The ladder-tail verdict travels on `FetchResult.wall`. A vendor wall with
/// enough prose passes the page classifier and its body is not error-page sized,
/// so `data.block` stays empty: the carried verdict is what must keep it out.
#[tokio::test]
async fn a_carried_wall_is_never_cached() {
    let (http, calls) = Tier::new("http", Ok(article()));
    let r = renderer_of(
        Tier {
            wall: Some(BlockOutcome {
                vendor: "cloudflare".into(),
                reason: "challenge".into(),
            }),
            ..http
        },
        vec![],
    );

    let first = run(&r, &req("carried-wall")).await;
    assert!(
        first.block.is_none(),
        "this fixture must pass the classifier"
    );
    let second = run(&r, &req("carried-wall")).await;
    assert!(!second.cached, "a walled page was replayed");
    assert_eq!(n(&calls), 2);
}

/// A selector that matched nothing describes the request, not the page: the
/// empty result must not be stored, or a caller without the selector would
/// replay it.
#[tokio::test]
async fn a_selector_that_matched_nothing_is_never_cached() {
    let (r, calls) = plain();

    let empty = run(
        &r,
        &ScrapeRequest {
            css_selector: Some("#does-not-exist".into()),
            ..req("selector")
        },
    )
    .await;
    assert!(
        empty.selector_matched_nothing(),
        "fixture must match nothing"
    );
    assert_eq!(n(&calls), 1);

    let plain_req = run(&r, &req("selector")).await;
    assert!(!plain_req.cached, "a no-match request seeded the cache");
    assert_eq!(n(&calls), 2);
    assert!(
        plain_req
            .markdown
            .as_deref()
            .is_some_and(|m| m.contains("Lume"))
    );
}

/// A rejected request is rejected on a warm URL too: validation runs before the
/// cache is consulted, and `actions` is not part of the key.
#[tokio::test]
async fn validation_runs_before_the_cache_is_read() {
    let (r, calls) = plain();
    run(&r, &req("validate")).await;
    assert_eq!(n(&calls), 1);

    let with_actions = ScrapeRequest {
        actions: Some(serde_json::json!([{ "type": "click" }])),
        ..req("validate")
    };
    assert!(matches!(
        try_run(&r, &with_actions).await,
        Err(CrwError::InvalidRequest(_))
    ));

    // A pin this deployment cannot serve still 400s instead of being answered
    // from an entry for the same URL.
    let unavailable_pin = ScrapeRequest {
        renderer: Some(RequestedRenderer::Camofox),
        ..req("validate")
    };
    assert!(matches!(
        try_run(&r, &unavailable_pin).await,
        Err(CrwError::InvalidRequest(_))
    ));
    assert_eq!(n(&calls), 1);
}

/// `raw_bytes` is take()n while a PDF is converted, so a replay has to carry its
/// own copy of the document or the second request converts nothing.
#[tokio::test]
async fn a_pdf_is_replayed_with_its_bytes() {
    let (http, calls) = Tier::new("http", Ok(String::new()));
    let r = renderer_of(
        Tier {
            pdf: Some(include_bytes!("fixtures/sample.pdf").to_vec()),
            ..http
        },
        vec![],
    );

    let first = run(&r, &req("pdf")).await;
    assert!(
        first
            .markdown
            .as_deref()
            .is_some_and(|m| m.contains("Hello fastCRW PDF parsing")),
        "fixture must convert: {:?}",
        first.markdown
    );
    let second = run(&r, &req("pdf")).await;
    assert!(second.cached);
    assert_eq!(n(&calls), 1);
    assert_eq!(second.markdown, first.markdown);
    assert_eq!(second.credit_cost, first.credit_cost);
}

/// A SPA whose cheap tiers return a thin shell escalates to camofox. The cached
/// entry is the page camofox produced, labelled as camofox, so a replay neither
/// reports the discarded tier nor re-runs the escalation.
#[tokio::test]
async fn an_escalated_page_is_cached_as_the_tier_that_produced_it() {
    let (http, http_calls) = Tier::new("http", Ok(spa_shell()));
    let (lp, lp_calls) = Tier::new("lightpanda", Ok(thin_render()));
    let (camofox, camofox_calls) = Tier::new("camofox", Ok(article()));
    let r = renderer_of(http, vec![lp, camofox]);

    let first = run(&r, &req("escalated")).await;
    assert!(!first.cached);
    assert_eq!(first.metadata.rendered_with.as_deref(), Some("camofox"));
    let tiers = (n(&http_calls), n(&lp_calls), n(&camofox_calls));
    assert!(
        tiers.2 >= 1,
        "the fixture must escalate to camofox: {tiers:?}"
    );

    let second = run(&r, &req("escalated")).await;
    assert!(second.cached, "an escalated page should be replayable");
    assert_eq!(
        second.metadata.rendered_with.as_deref(),
        Some("camofox"),
        "the replay must name the tier that produced the page"
    );
    assert_eq!(
        (n(&http_calls), n(&lp_calls), n(&camofox_calls)),
        tiers,
        "a hit must not re-run the ladder or the escalation"
    );
    assert_eq!(second.markdown, first.markdown);
}

/// An escalation that failed leaves the thin prior body in place. That body is
/// a stand-in, so it is never pinned.
#[tokio::test]
async fn a_failed_escalation_is_never_cached() {
    let (http, _) = Tier::new("http", Ok(spa_shell()));
    let (lp, _) = Tier::new("lightpanda", Ok(thin_render()));
    let (camofox, camofox_calls) = Tier::new("camofox", Err("navigation failed".to_string()));
    let r = renderer_of(http, vec![lp, camofox]);

    let first = run(&r, &req("escalation-failed")).await;
    assert!(
        first
            .warning
            .as_deref()
            .is_some_and(|w| w.contains(crw_renderer::JS_ESCALATION_FAILED)),
        "fixture must exercise the failed escalation: {:?}",
        first.warning
    );
    let before = n(&camofox_calls);
    let second = run(&r, &req("escalation-failed")).await;
    assert!(!second.cached);
    assert!(
        n(&camofox_calls) > before,
        "the failed tier was not retried"
    );
}

/// A deployment with no tier above LightPanda skips the escalation with a
/// warning and ships the thin page. It must not be pinned either.
#[tokio::test]
async fn a_skipped_escalation_is_never_cached() {
    let (http, _) = Tier::new("http", Ok(spa_shell()));
    let (lp, lp_calls) = Tier::new("lightpanda", Ok(thin_render()));
    let r = renderer_of(http, vec![lp]);

    run(&r, &req("escalation-skipped")).await;
    let before = n(&lp_calls);
    let second = run(&r, &req("escalation-skipped")).await;
    assert!(!second.cached);
    assert!(n(&lp_calls) > before);
}

/// The renderer ladder itself tries JS (the page asks for it), fails, and falls
/// back to the HTTP body with a `js_escalation_failed` warning. The body is
/// substantial, so only the warning says it is a stand-in for a page we could
/// not render: it must not be pinned for the window.
#[tokio::test]
async fn a_body_left_by_a_failed_js_ladder_is_never_cached() {
    let body = format!(
        "{}<noscript>You need to enable JavaScript to run this app.</noscript>\
         <div id=\"root\"></div><script src=\"/static/main.js\"></script>",
        article()
    );
    let (http, _) = Tier::new("http", Ok(body));
    let (camofox, camofox_calls) = Tier::new("camofox", Err("navigation failed".to_string()));
    let r = renderer_of(http, vec![camofox]);

    let first = run(&r, &req("ladder-failed")).await;
    assert!(
        first
            .warning
            .as_deref()
            .is_some_and(|w| w.contains(crw_renderer::JS_ESCALATION_FAILED)),
        "fixture must exercise the failed ladder: {:?}",
        first.warning
    );
    assert!(
        first.markdown.as_deref().is_some_and(|m| m.len() > 2_000),
        "fixture must not be thin, or the thin guard would hide the warning guard"
    );
    let before = n(&camofox_calls);
    let second = run(&r, &req("ladder-failed")).await;
    assert!(!second.cached, "a stand-in body was replayed");
    assert!(
        n(&camofox_calls) > before,
        "the failed ladder was not retried"
    );
}

/// The accepted escalation hands `fetch_result` its body AND its wall verdict and
/// truncation flag, and the entry is assembled from that merged view. A vendor
/// wall with enough prose still extracts as a page, so the carried verdict is
/// what has to keep the merged entry out.
#[tokio::test]
async fn an_escalated_render_that_carries_a_wall_is_never_cached() {
    let (http, _) = Tier::new("http", Ok(spa_shell()));
    let (lp, _) = Tier::new("lightpanda", Ok(thin_render()));
    let (camofox, camofox_calls) = Tier::new("camofox", Ok(article()));
    let r = renderer_of(
        http,
        vec![
            lp,
            Tier {
                wall: Some(BlockOutcome {
                    vendor: "cloudflare".into(),
                    reason: "challenge".into(),
                }),
                ..camofox
            },
        ],
    );

    let first = run(&r, &req("escalated-wall")).await;
    assert_eq!(first.metadata.rendered_with.as_deref(), Some("camofox"));
    let before = n(&camofox_calls);
    let second = run(&r, &req("escalated-wall")).await;
    assert!(!second.cached, "a walled escalation was replayed");
    assert!(n(&camofox_calls) > before);
}

/// Same, for a partial DOM: the escalated render's truncation flag moves onto the
/// merged view, and a cut-short page must not be pinned.
#[tokio::test]
async fn an_escalated_render_that_was_cut_short_is_never_cached() {
    let (http, _) = Tier::new("http", Ok(spa_shell()));
    let (lp, _) = Tier::new("lightpanda", Ok(thin_render()));
    let (camofox, camofox_calls) = Tier::new("camofox", Ok(article()));
    let r = renderer_of(
        http,
        vec![
            lp,
            Tier {
                truncated: true,
                ..camofox
            },
        ],
    );

    let first = run(&r, &req("escalated-cut")).await;
    assert_eq!(first.metadata.rendered_with.as_deref(), Some("camofox"));
    let before = n(&camofox_calls);
    let second = run(&r, &req("escalated-cut")).await;
    assert!(!second.cached, "a truncated escalation was replayed");
    assert!(n(&camofox_calls) > before);
}

/// A challenge the page classifier recognises (here Cloudflare's loader config)
/// is stamped on `data.block` and shipped as a wall, so it must not be replayed
/// to every later caller.
#[tokio::test]
async fn a_page_the_classifier_calls_a_wall_is_never_cached() {
    let body = format!(
        "{}<script>window._cf_chl_opt={{cvId:'3',cType:'managed'}};</script>\
         <title>Just a moment...</title>",
        article()
    );
    let (http, calls) = Tier::new("http", Ok(body));
    let r = renderer_of(http, vec![]);

    let first = run(&r, &req("classified-wall")).await;
    assert!(first.block.is_some(), "fixture must classify as a wall");
    let second = run(&r, &req("classified-wall")).await;
    assert!(!second.cached, "a classified wall was replayed");
    assert_eq!(n(&calls), 2);
}
