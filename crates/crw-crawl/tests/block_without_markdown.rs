//! A 403 page with real content gets the same block verdict whether or not the
//! request asks for markdown. A `rawHtml`-only request carries no markdown, and
//! the block check used to read that as an empty page, so every such 403 came
//! back `generic_block` while the same page with `formats: ["markdown"]` passed.

use std::sync::Arc;

use crw_core::config::{ExtractionConfig, RendererConfig, StealthConfig};
use crw_core::types::{ScrapeData, ScrapeRequest};
use crw_crawl::single::scrape_url;
use crw_renderer::FallbackRenderer;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const UA: &str = "crw-test/0.0";

fn article() -> String {
    let para = "Members of the archive can read the full history of the project here. ";
    format!(
        "<html><head><title>Archive</title></head><body>\
         <main><article><h1>Project history</h1><p>{}</p></article></main></body></html>",
        para.repeat(60)
    )
}

async fn scrape(formats: serde_json::Value, route: &str) -> ScrapeData {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(403).set_body_raw(article(), "text/html; charset=utf-8"),
        )
        .mount(&server)
        .await;
    let req: ScrapeRequest = serde_json::from_value(serde_json::json!({
        "url": format!("{}{route}", server.uri()),
        "formats": formats,
        "renderJs": false,
    }))
    .expect("request");
    let renderer = Arc::new(
        FallbackRenderer::new(
            &RendererConfig::default(),
            UA,
            None,
            &StealthConfig::default(),
        )
        .expect("renderer"),
    );
    scrape_url(
        &req,
        &renderer,
        None,
        &ExtractionConfig::default(),
        UA,
        false,
        Some(false),
        crw_core::deadline::Deadline::from_request_ms(60_000),
    )
    .await
    .expect("scrape")
}

#[tokio::test]
async fn a_real_403_page_is_not_a_wall_without_markdown() {
    let raw = scrape(serde_json::json!(["rawHtml"]), "/raw").await;
    assert!(raw.block.is_none(), "rawHtml: {:?}", raw.block);
    assert!(
        raw.raw_html
            .as_deref()
            .is_some_and(|h| h.contains("Project history"))
    );

    let md = scrape(serde_json::json!(["markdown"]), "/md").await;
    assert!(md.block.is_none(), "markdown: {:?}", md.block);
}
