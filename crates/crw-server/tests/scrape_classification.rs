//! `/v1/scrape` and `/v2/scrape` must classify the same origin response the same
//! way: a real page succeeds, a wall is `anti_bot` with its body cleared, an
//! empty page is `no_usable_content`, and a CDN answering for a dead origin is
//! `target_unreachable`. The SaaS bills from `success`, so the two surfaces
//! disagreeing is a billing bug.
//!
//! The one deliberate split is a plain origin 4xx/5xx page: `/v1` calls it
//! `http_error`, `/v2` returns it as a `success:true` document carrying the
//! status, as Firecrawl does.
//!
//! Own test binary because it sets `CRW_ALLOW_LOOPBACK_FOR_TESTS`, which is
//! process-global.

use axum_test::TestServer;
use crw_core::config::{AppConfig, RendererMode};
use crw_server::app::create_app;
use crw_server::state::AppState;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn html(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .insert_header("content-type", "text/html")
        .set_body_string(body.to_string())
}

async fn origin() -> MockServer {
    let server = MockServer::start().await;
    let article = format!(
        "<html><head><title>Article</title></head><body><article><h1>Article</h1>{}</article></body></html>",
        "<p>A real paragraph of article prose that a reader came here for.</p>".repeat(20)
    );
    let routes = [
        ("/ok", html(200, &article)),
        (
            "/missing",
            html(
                404,
                "<html><body><h1>Not Found</h1><p>No such page.</p></body></html>",
            ),
        ),
        (
            "/wall",
            html(
                403,
                "<html><head><title>example.com</title></head><body>\
                 <script src=\"https://geo.captcha-delivery.com/captcha/\"></script>\
                 <p>Please enable JS and disable any ad blocker</p></body></html>",
            ),
        ),
        ("/empty", html(200, "")),
        (
            "/moved",
            ResponseTemplate::new(302).insert_header("location", "/ok"),
        ),
        (
            "/dead",
            html(
                522,
                "<html><body><h1>Connection timed out</h1></body></html>",
            ),
        ),
    ];
    for (p, resp) in routes {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(resp)
            .mount(&server)
            .await;
    }
    server
}

fn app() -> TestServer {
    // SAFETY: one binary per tests/*.rs, set before any fetcher is built.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") };
    let mut cfg = AppConfig::default();
    cfg.renderer.mode = RendererMode::None;
    TestServer::new(create_app(AppState::new(cfg).expect("AppState")))
}

async fn v1(server: &TestServer, url: &str) -> (u16, Value) {
    let resp = server.post("/v1/scrape").json(&json!({ "url": url })).await;
    (resp.status_code().as_u16(), resp.json())
}

async fn v2(server: &TestServer, url: &str) -> (u16, Value) {
    let resp = server.post("/v2/scrape").json(&json!({ "url": url })).await;
    (resp.status_code().as_u16(), resp.json())
}

#[tokio::test]
async fn v1_and_v2_classify_origin_responses_alike() {
    let origin = origin().await;
    let server = app();
    let url = |p: &str| format!("{}{p}", origin.uri());

    // (path, v1 success, v1 error_code, v2 success)
    let cases = [
        ("/ok", true, None, true),
        // The deliberate split: Firecrawl returns an origin 404 as a document.
        ("/missing", false, Some("http_error"), true),
        ("/wall", false, Some("anti_bot"), false),
        ("/empty", false, Some("no_usable_content"), false),
    ];
    for (p, success, code, v2_success) in cases {
        let (_, one) = v1(&server, &url(p)).await;
        assert_eq!(one["success"], success, "v1 {p}: {one}");
        assert_eq!(one["error_code"].as_str(), code, "v1 {p}: {one}");

        let (_, two) = v2(&server, &url(p)).await;
        assert_eq!(two["success"], v2_success, "v2 {p}: {two}");
        if !v2_success {
            assert!(two["error"].is_string(), "v2 {p} must say why: {two}");
        }
    }

    // A wall's challenge text is not content on either surface.
    let (_, one) = v1(&server, &url("/wall")).await;
    assert!(
        one["data"]["markdown"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "v1 wall body must be cleared: {one}"
    );
    let (_, two) = v2(&server, &url("/wall")).await;
    assert!(
        two["data"]["markdown"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "v2 wall body must be cleared: {two}"
    );
}

#[tokio::test]
async fn cdn_answering_for_a_dead_origin_is_unreachable_on_both_surfaces() {
    let origin = origin().await;
    let server = app();
    let url = format!("{}/dead", origin.uri());

    let (status, one) = v1(&server, &url).await;
    assert_eq!(status, 422, "{one}");
    assert_eq!(one["error_code"], "target_unreachable", "{one}");

    let (status, two) = v2(&server, &url).await;
    assert_eq!(status, 422, "{two}");
    assert_eq!(two["success"], false, "{two}");
    // Firecrawl's taxonomy as well as ours; never its 200 DNS answer.
    assert_eq!(two["code"], "SCRAPE_SITE_ERROR", "{two}");
    assert_eq!(two["error_code"], "target_unreachable", "{two}");
}

/// An origin 4xx is a document on `/v2` whatever its size: `success:true`, the
/// status and its reason phrase in the metadata, and billed.
#[tokio::test]
async fn v2_returns_an_origin_error_page_as_a_billed_document() {
    let origin = origin().await;
    let server = app();
    let (status, body) = v2(&server, &format!("{}/missing", origin.uri())).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["success"], true, "{body}");
    let meta = &body["data"]["metadata"];
    assert_eq!(meta["statusCode"], 404, "{body}");
    assert_eq!(meta["error"], "Not Found", "{body}");
    assert_eq!(meta["creditsUsed"], 1, "{body}");
    assert!(
        body["data"]["markdown"]
            .as_str()
            .is_some_and(|m| m.contains("No such page")),
        "the error page stays readable: {body}"
    );
}

/// A wall stays a failure at its 403: no body, nothing billed. Firecrawl would
/// bill it as a document.
#[tokio::test]
async fn v2_fails_a_wall_served_with_a_4xx_unbilled() {
    let origin = origin().await;
    let server = app();
    let (status, body) = v2(&server, &format!("{}/wall", origin.uri())).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["success"], false, "{body}");
    assert_eq!(body["data"]["metadata"]["creditsUsed"], 0, "{body}");
    assert!(
        body["data"]["markdown"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "{body}"
    );
}

/// `metadata.url` is where a redirect landed; `sourceURL` stays what was asked.
#[tokio::test]
async fn v2_reports_the_redirect_target_in_metadata_url() {
    let origin = origin().await;
    let server = app();
    let asked = format!("{}/moved", origin.uri());
    let (_, body) = v2(&server, &asked).await;

    assert_eq!(body["success"], true, "{body}");
    let meta = &body["data"]["metadata"];
    assert_eq!(meta["sourceURL"], asked, "{body}");
    assert_eq!(meta["url"], format!("{}/ok", origin.uri()), "{body}");
}

/// A 200 with nothing in it is an engine failure, not a 200 carrying
/// `success:false`.
#[tokio::test]
async fn v2_answers_an_empty_200_with_all_engines_failed() {
    let origin = origin().await;
    let server = app();
    let (status, body) = v2(&server, &format!("{}/empty", origin.uri())).await;

    assert_eq!(status, 500, "{body}");
    assert_eq!(body["success"], false, "{body}");
    assert_eq!(body["code"], "SCRAPE_ALL_ENGINES_FAILED", "{body}");
}
