//! `[page_cache] enabled = false` turns the cache off for everyone, including a
//! request that sends an explicit `maxAge`.
//!
//! Own test binary because the settings are process-global and first-call-wins:
//! `configure` has to run before any other test in the process touches the cache.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crw_core::Deadline;
use crw_core::config::{
    ExtractionConfig, PageCacheConfig, RendererConfig, RendererMode, StealthConfig,
};
use crw_core::error::CrwResult;
use crw_core::types::{FetchResult, OutputFormat, ScrapeRequest};
use crw_crawl::single::scrape_url;
use crw_renderer::FallbackRenderer;
use crw_renderer::traits::PageFetcher;

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
                "<html><body><main><h1>Coffee</h1><p>{}</p></main></body></html>",
                para.repeat(40)
            ),
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

#[tokio::test]
async fn a_disabled_cache_fetches_live_whatever_the_request_says() {
    crw_crawl::page_cache::configure(&PageCacheConfig {
        enabled: false,
        ..Default::default()
    });

    let calls = Arc::new(AtomicUsize::new(0));
    let cfg = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    let renderer = Arc::new(
        FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default())
            .expect("renderer")
            .with_fetchers(Arc::new(Http(calls.clone())), vec![]),
    );

    for max_age in [None, Some(3_600_000)] {
        let req = ScrapeRequest {
            url: "https://disabled.cache.test/page".into(),
            formats: vec![OutputFormat::Markdown],
            max_age,
            ..Default::default()
        };
        for _ in 0..2 {
            let data = scrape_url(
                &req,
                &renderer,
                None,
                &ExtractionConfig::default(),
                "crw-test",
                false,
                None,
                Deadline::from_request_ms(60_000),
            )
            .await
            .expect("scrape");
            assert!(!data.cached, "a disabled cache served a hit");
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4, "every scrape fetches live");
}
