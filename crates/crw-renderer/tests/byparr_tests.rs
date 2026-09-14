//! Behavioural tests for the Byparr challenge-solver tier against a mock
//! `POST /v1`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crw_core::Deadline;
use crw_core::error::CrwError;
use crw_renderer::byparr::ByparrRenderer;
use crw_renderer::clearance::ClearanceCache;
use crw_renderer::traits::PageFetcher;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HTML: &str = "<html><head><title>Real page</title></head><body><h1>solved</h1></body></html>";
/// A public IP literal, so the final-URL check needs no DNS.
const PUBLIC_FINAL_URL: &str = "https://93.184.215.14/";

fn far_expiry() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        + 3_600.0
}

fn solved(final_url: &str) -> Value {
    json!({
        "status": "ok",
        "message": "Success",
        "solution": {
            "url": final_url,
            "status": 200,
            "cookies": [
                { "name": "cf_clearance", "value": "abc", "domain": ".example.com", "path": "/",
                  "expires": far_expiry(), "httpOnly": true, "secure": true, "sameSite": "None" },
                { "name": "_ga", "value": "tracker", "domain": ".other.com", "path": "/",
                  "expires": far_expiry(), "httpOnly": false, "secure": false, "sameSite": "Lax" }
            ],
            "userAgent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:151.0) Gecko/20100101 Firefox/151.0",
            "headers": {},
            "response": HTML,
            "contentType": "text/html"
        },
        "startTimestamp": 0,
        "endTimestamp": 1,
        "version": "3.0.4"
    })
}

fn deadline() -> Deadline {
    Deadline::now_plus(Duration::from_secs(60))
}

#[tokio::test]
async fn solve_returns_page_and_caches_host_clearance() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(solved(PUBLIC_FINAL_URL)))
        .expect(1)
        .mount(&server)
        .await;
    let cache = Arc::new(ClearanceCache::with_defaults());
    let renderer = ByparrRenderer::new(&server.uri(), Duration::from_secs(30), 2)
        .with_clearance_cache(Arc::clone(&cache));

    let result = renderer
        .fetch("https://example.com/", &HashMap::new(), None, deadline())
        .await
        .expect("solve succeeds");

    assert_eq!(result.rendered_with.as_deref(), Some("byparr"));
    assert_eq!(result.status_code, 200);
    assert!(result.html.contains("solved"));
    assert_eq!(result.final_url.as_deref(), Some(PUBLIC_FINAL_URL));

    let req: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(req["cmd"], "request.get");
    assert_eq!(req["url"], "https://example.com/");
    let max_timeout = req["maxTimeout"].as_u64().unwrap();
    assert!(
        (1_000..=30_000).contains(&max_timeout),
        "maxTimeout in ms, capped at the tier timeout: {max_timeout}"
    );

    let entry = cache.get("example.com").await.expect("clearance cached");
    assert!(entry.user_agent.contains("Firefox/151.0"));
    assert_eq!(
        entry
            .cookies
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["cf_clearance"],
        "third-party cookies are not cached for the host"
    );
}

#[tokio::test]
async fn internal_final_url_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(solved("http://169.254.169.254/latest/")),
        )
        .mount(&server)
        .await;
    let cache = Arc::new(ClearanceCache::with_defaults());
    let renderer = ByparrRenderer::new(&server.uri(), Duration::from_secs(30), 2)
        .with_clearance_cache(Arc::clone(&cache));

    let err = renderer
        .fetch("https://example.com/", &HashMap::new(), None, deadline())
        .await
        .expect_err("a page that landed on the metadata endpoint is not returned");

    assert!(err.to_string().contains("blocked destination"), "{err}");
    assert!(cache.is_empty(), "nothing is cached from a refused solve");
}

#[tokio::test]
async fn solve_timeout_maps_to_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(408).set_body_json(
            json!({ "detail": "Timed out while loading the page or solving the challenge" }),
        ))
        .mount(&server)
        .await;
    let renderer = ByparrRenderer::new(&server.uri(), Duration::from_secs(30), 2);

    let err = renderer
        .fetch("https://example.com/", &HashMap::new(), None, deadline())
        .await
        .expect_err("408 is a failed solve");

    assert!(matches!(err, CrwError::Timeout(_)), "{err:?}");
}

#[tokio::test]
async fn unreachable_target_reads_as_navigation_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(502).set_body_json(
            json!({ "detail": "Could not reach the target: NS_ERROR_UNKNOWN_HOST" }),
        ))
        .mount(&server)
        .await;
    let renderer = ByparrRenderer::new(&server.uri(), Duration::from_secs(30), 2);

    let err = renderer
        .fetch(
            "https://example.invalid/",
            &HashMap::new(),
            None,
            deadline(),
        )
        .await
        .expect_err("502 is a failed navigation");

    let msg = err.to_string();
    assert!(msg.contains("navigation failed"), "{msg}");
    assert!(msg.contains("NS_ERROR_UNKNOWN_HOST"), "{msg}");
}

#[tokio::test]
async fn error_status_in_body_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "error",
            "message": "Invalid request",
            "solution": { "url": "https://example.com/", "status": 500 },
            "startTimestamp": 0
        })))
        .mount(&server)
        .await;
    let renderer = ByparrRenderer::new(&server.uri(), Duration::from_secs(30), 2);

    let err = renderer
        .fetch("https://example.com/", &HashMap::new(), None, deadline())
        .await
        .expect_err("status error is not a page");

    assert!(err.to_string().contains("Invalid request"), "{err}");
}

#[tokio::test]
async fn short_budget_skips_the_solve() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(solved(PUBLIC_FINAL_URL)))
        .expect(0)
        .mount(&server)
        .await;
    let renderer = ByparrRenderer::new(&server.uri(), Duration::from_secs(30), 2);

    let err = renderer
        .fetch(
            "https://example.com/",
            &HashMap::new(),
            None,
            Deadline::now_plus(Duration::from_secs(3)),
        )
        .await
        .expect_err("a solve cannot finish in 3 s");

    assert!(matches!(err, CrwError::Timeout(_)), "{err:?}");
}

/// With one permit, three solves that each take 300 ms run back to back.
#[tokio::test]
async fn concurrent_solves_are_capped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(solved(PUBLIC_FINAL_URL))
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    let renderer = Arc::new(ByparrRenderer::new(
        &server.uri(),
        Duration::from_secs(30),
        1,
    ));

    let started = std::time::Instant::now();
    let tasks: Vec<_> = (0..3)
        .map(|_| {
            let r = Arc::clone(&renderer);
            tokio::spawn(async move {
                r.fetch("https://example.com/", &HashMap::new(), None, deadline())
                    .await
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().expect("each solve succeeds");
    }

    assert!(
        started.elapsed() >= Duration::from_millis(850),
        "solves overlapped: {:?}",
        started.elapsed()
    );
}
