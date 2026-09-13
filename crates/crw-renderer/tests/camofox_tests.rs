#![cfg(feature = "camofox")]
//! Behavioural tests for the Camofox (camofox-browser REST) renderer tier.
//! A small axum app emulates the camofox-browser `:9377` REST surface so we can
//! assert the navigate→wait→evaluate→close round-trip and `FetchResult` mapping
//! without a live Firefox.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use crw_core::Deadline;
use crw_renderer::camofox::CamofoxRenderer;
use crw_renderer::traits::PageFetcher;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const RENDERED_HTML: &str = "<html><body><h1>camofox rendered</h1></body></html>";

async fn create_tab(Json(body): Json<Value>) -> impl IntoResponse {
    // The real camofox-browser requires both userId and sessionKey.
    if body.get("sessionKey").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "userId and sessionKey required" })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "tabId": "tab-1", "sessionKey": "s-1" })),
    )
}

async fn navigate(Path(_id): Path<String>, Json(body): Json<Value>) -> impl IntoResponse {
    if body.get("url").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "url required" })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "url": body["url"] })),
    )
}

/// camofox's navigate on a huge page: the navigation itself succeeded, but the
/// route's post-navigation ARIA snapshot timed out and it answers 500 with a
/// sanitized body.
async fn navigate_snapshot_timeout(
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> impl IntoResponse {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Internal server error" })),
    )
}

/// Evaluate for a tab whose navigation committed: `location.href` answers the
/// target, anything else the rendered document.
async fn evaluate_committed(Path(_id): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    if body["expression"].as_str() == Some("location.href") {
        return Json(json!({
            "ok": true, "result": "https://example.com/huge", "resultType": "string", "truncated": false
        }));
    }
    Json(json!({ "ok": true, "result": RENDERED_HTML, "resultType": "string", "truncated": false }))
}

/// Evaluate for a tab whose navigation never committed (still about:blank).
async fn evaluate_blank(Path(_id): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    if body["expression"].as_str() == Some("location.href") {
        return Json(
            json!({ "ok": true, "result": "about:blank", "resultType": "string", "truncated": false }),
        );
    }
    Json(json!({
        "ok": true, "result": "<html><head></head><body></body></html>", "resultType": "string", "truncated": false
    }))
}

async fn wait(Path(_id): Path<String>, Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// A `/tabs` handler that hangs far longer than any test deadline — models a
/// stalled camofox navigate (Google `/sorry` interstitial, dead upstream).
async fn create_tab_stalls(Json(_body): Json<Value>) -> impl IntoResponse {
    tokio::time::sleep(Duration::from_secs(30)).await;
    (StatusCode::OK, Json(json!({ "tabId": "tab-slow" })))
}

/// A `/tabs` handler that fails the way camofox does when a persistent profile
/// is pinned to an older Camoufox build: HTTP 500 with the reason in `error`.
async fn create_tab_profile_mismatch(Json(_body): Json<Value>) -> impl IntoResponse {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "Profile for user \"crw\" was created with Camoufox 135.0.1-beta.24, but the current version is 152.0.4-beta.28"
        })),
    )
}

/// A `/tabs` handler failing through a proxy: non-JSON body that must not be
/// echoed into the renderer error.
async fn create_tab_html_error(Json(_body): Json<Value>) -> impl IntoResponse {
    (
        StatusCode::BAD_GATEWAY,
        "<html><body>Bad Gateway at /internal/x</body></html>",
    )
}

/// `/tabs` that fails the first two creates the way camofox does right after a
/// context teardown (`window is null`), then succeeds — the transient the
/// renderer must ride out.
static FLAKY_CREATES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
async fn create_tab_flaky(Json(body): Json<Value>) -> axum::response::Response {
    let n = FLAKY_CREATES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if n < 2 {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "browserContext.newPage: Protocol error (Browser.newPage): can't access property \"delayedStartupPromise\", window is null"
            })),
        )
            .into_response();
    }
    create_tab(Json(body)).await.into_response()
}

async fn evaluate(Path(_id): Path<String>, Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "result": RENDERED_HTML,
        "resultType": "string",
        "truncated": false,
    }))
}

/// A document larger than camofox's 1 MiB single-result cap: the plain
/// outerHTML evaluate answers with the truncation placeholder, and the
/// renderer must fall back to slicing. ASCII only, so byte, char and UTF-16
/// offsets coincide in the mock.
fn big_html() -> &'static String {
    static BIG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BIG.get_or_init(|| {
        let mut s = String::from("<html><body>");
        while s.len() < 700_000 {
            s.push_str("<p>chunked-render-payload-0123456789</p>");
        }
        s.push_str("<h1>the end</h1></body></html>");
        s
    })
}

async fn evaluate_big(Path(_id): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    let expr = body["expression"].as_str().unwrap_or_default();
    let doc = big_html();
    if expr == "document.documentElement.outerHTML" {
        return Json(json!({
            "ok": true,
            "result": format!("[Truncated: result was {} bytes, max 1048576]", doc.len() + 2),
            "resultType": "string",
            "truncated": true,
        }));
    }
    if expr.contains("outerHTML.length") {
        return Json(
            json!({ "ok": true, "result": doc.len().to_string(), "resultType": "string", "truncated": false }),
        );
    }
    // `(function(s,a,b){...})(document.documentElement.outerHTML,A,B)`
    let args = expr
        .rsplit_once("outerHTML,")
        .map(|(_, tail)| tail.trim_end_matches(')'))
        .unwrap();
    let (a, b) = args.split_once(',').unwrap();
    let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
    Json(
        json!({ "ok": true, "result": &doc[a..b.min(doc.len())], "resultType": "string", "truncated": false }),
    )
}

async fn close_tab(Path(_id): Path<String>, Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "engine": "camoufox", "browserConnected": true }))
}

async fn spawn_camofox_mock() -> String {
    let app = Router::new()
        .route("/tabs", post(create_tab))
        .route("/tabs/{id}/navigate", post(navigate))
        .route("/tabs/{id}/wait", post(wait))
        .route("/tabs/{id}/evaluate", post(evaluate))
        .route("/tabs/{id}", delete(close_tab))
        .route("/health", get(health));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn deadline() -> Deadline {
    Deadline::now_plus(Duration::from_secs(30))
}

#[tokio::test]
async fn fetch_returns_evaluated_html() {
    let base = spawn_camofox_mock().await;
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(10));

    let result = renderer
        .fetch("https://example.com", &HashMap::new(), None, deadline())
        .await
        .expect("camofox fetch should succeed against the mock");

    assert_eq!(result.status_code, 200);
    assert!(
        result.html.contains("camofox rendered"),
        "expected evaluated outerHTML, got: {}",
        result.html
    );
    assert_eq!(result.rendered_with.as_deref(), Some("camofox"));
}

#[tokio::test]
async fn name_and_js_support() {
    let renderer = CamofoxRenderer::new(
        "camofox",
        "http://127.0.0.1:1",
        None,
        Duration::from_secs(5),
    );
    assert_eq!(renderer.name(), "camofox");
    assert!(renderer.supports_js());
}

#[tokio::test]
async fn is_available_reads_health() {
    let base = spawn_camofox_mock().await;
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(5));
    assert!(renderer.is_available().await);
}

#[tokio::test]
async fn fetch_bounded_by_deadline_not_client_timeout() {
    // The client timeout (10s) is far longer than the caller deadline (600ms).
    // A stalled navigate must surface as a deadline-bounded failure quickly,
    // NOT run for the full client timeout — the PageFetcher contract the
    // failover ladder relies on to move to the next tier / return 504.
    let app = Router::new().route("/tabs", post(create_tab_stalls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(10));

    let started = std::time::Instant::now();
    let res = renderer
        .fetch(
            "https://example.com",
            &HashMap::new(),
            None,
            Deadline::now_plus(Duration::from_millis(600)),
        )
        .await;
    let elapsed = started.elapsed();

    assert!(
        matches!(res, Err(crw_core::error::CrwError::Timeout(_))),
        "a stalled navigate must surface as Timeout (→504), got {res:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "must be bounded by the ~600ms deadline, not the 10s client timeout; took {elapsed:?}"
    );
}

#[tokio::test]
async fn fetch_fails_when_deadline_expired() {
    let renderer = CamofoxRenderer::new(
        "camofox",
        "http://127.0.0.1:1",
        None,
        Duration::from_secs(5),
    );
    let expired = Deadline::now_plus(Duration::from_millis(0));
    let res = renderer
        .fetch("https://example.com", &HashMap::new(), None, expired)
        .await;
    assert!(
        res.is_err(),
        "expired deadline should short-circuit before any HTTP call"
    );
}

#[tokio::test]
async fn fetch_error_carries_camofox_message() {
    // A failed camofox call must surface the server's own `error` text, not
    // just the status — it is what tells a profile-version pin apart from a
    // crashed browser.
    let app = Router::new().route("/tabs", post(create_tab_profile_mismatch));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(5));

    let err = renderer
        .fetch("https://example.com", &HashMap::new(), None, deadline())
        .await
        .expect_err("500 from /tabs must fail the fetch");
    let msg = err.to_string();
    assert!(msg.contains("camofox /tabs returned 500"), "{msg}");
    assert!(
        msg.contains("was created with Camoufox 135.0.1-beta.24"),
        "{msg}"
    );
}

#[tokio::test]
async fn fetch_error_omits_non_json_body() {
    let app = Router::new().route("/tabs", post(create_tab_html_error));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(5));

    let err = renderer
        .fetch("https://example.com", &HashMap::new(), None, deadline())
        .await
        .expect_err("502 from /tabs must fail the fetch");
    let msg = err.to_string();
    assert!(
        msg.ends_with("camofox /tabs returned 502 Bad Gateway"),
        "{msg}"
    );
    assert!(!msg.contains("<html"), "{msg}");
}

#[tokio::test]
async fn fetch_retries_transient_tab_create_failure() {
    FLAKY_CREATES.store(0, std::sync::atomic::Ordering::SeqCst);
    let app = Router::new()
        .route("/tabs", post(create_tab_flaky))
        .route("/tabs/{id}/navigate", post(navigate))
        .route("/tabs/{id}/wait", post(wait))
        .route("/tabs/{id}/evaluate", post(evaluate))
        .route("/tabs/{id}", delete(close_tab));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(5));

    let result = renderer
        .fetch("https://example.com", &HashMap::new(), None, deadline())
        .await
        .expect("two transient 500s on /tabs must be retried through");
    assert!(result.html.contains("camofox rendered"));
    assert_eq!(FLAKY_CREATES.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn fetch_gives_up_on_persistent_tab_create_failure() {
    // Always 500: after the retry budget the error surfaces (bounded, not a hang).
    let app = Router::new().route("/tabs", post(create_tab_profile_mismatch));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(5));

    let started = std::time::Instant::now();
    let err = renderer
        .fetch("https://example.com", &HashMap::new(), None, deadline())
        .await
        .expect_err("persistent 500 must fail");
    assert!(
        err.to_string().contains("camofox /tabs returned 500"),
        "{err}"
    );
    // 3 retries with 0.5 s / 1 s / 2 s pauses ≈ 3.5 s; anything near the 30 s
    // deadline would mean the retry loop is not bounded by the attempt count.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "retries must stay bounded"
    );
}

/// A pinned JS renderer implies `renderJs=true`. When the HTTP tier fails on
/// that path (here: an origin slower than the HTTP timeout) the request must
/// escalate to the renderer, not surface the HTTP tier's error.
#[tokio::test]
async fn render_js_true_escalates_when_http_tier_fails() {
    use crw_core::config::{CamofoxEndpoint, RendererConfig, RendererMode, StealthConfig};
    use crw_renderer::FallbackRenderer;
    use wiremock::matchers::{method, path as wpath};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // SAFETY: this test binary owns its process env.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") };
    let camofox = spawn_camofox_mock().await;
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wpath("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<html>too late</html>")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&origin)
        .await;

    let cfg = RendererConfig {
        mode: RendererMode::Camofox,
        camofox: Some(CamofoxEndpoint {
            base_url: camofox,
            api_key: None,
        }),
        http_timeout_ms: Some(300),
        ..Default::default()
    };
    let renderer = FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default())
        .expect("camofox-mode renderer builds");

    let result = renderer
        .fetch(
            &format!("{}/slow", origin.uri()),
            &HashMap::new(),
            Some(true),
            None,
            Some("camofox"),
            Deadline::now_plus(Duration::from_secs(30)),
        )
        .await
        .expect("HTTP-tier timeout must escalate to the pinned renderer");
    assert_eq!(result.rendered_with.as_deref(), Some("camofox"));
    assert!(result.html.contains("camofox rendered"));
}

#[tokio::test]
async fn fetch_reassembles_document_over_camofox_result_cap() {
    let app = Router::new()
        .route("/tabs", post(create_tab))
        .route("/tabs/{id}/navigate", post(navigate))
        .route("/tabs/{id}/wait", post(wait))
        .route("/tabs/{id}/evaluate", post(evaluate_big))
        .route("/tabs/{id}", delete(close_tab));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(10));

    let result = renderer
        .fetch("https://example.com/big", &HashMap::new(), None, deadline())
        .await
        .expect("a document over the evaluate cap must be fetched in slices");
    assert_eq!(
        result.html.len(),
        big_html().len(),
        "reassembled document must be complete"
    );
    assert_eq!(&result.html, big_html());
    assert!(result.html.ends_with("<h1>the end</h1></body></html>"));
}

#[tokio::test]
async fn fetch_continues_when_only_the_post_navigation_snapshot_failed() {
    let app = Router::new()
        .route("/tabs", post(create_tab))
        .route("/tabs/{id}/navigate", post(navigate_snapshot_timeout))
        .route("/tabs/{id}/wait", post(wait))
        .route("/tabs/{id}/evaluate", post(evaluate_committed))
        .route("/tabs/{id}", delete(close_tab));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(5));

    let result = renderer
        .fetch(
            "https://example.com/huge",
            &HashMap::new(),
            None,
            deadline(),
        )
        .await
        .expect("a navigate failure after the page committed must not fail the render");
    assert!(result.html.contains("camofox rendered"));
}

#[tokio::test]
async fn fetch_fails_when_navigate_failed_and_tab_stayed_blank() {
    let app = Router::new()
        .route("/tabs", post(create_tab))
        .route("/tabs/{id}/navigate", post(navigate_snapshot_timeout))
        .route("/tabs/{id}/wait", post(wait))
        .route("/tabs/{id}/evaluate", post(evaluate_blank))
        .route("/tabs/{id}", delete(close_tab));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let renderer = CamofoxRenderer::new("camofox", &base, None, Duration::from_secs(5));

    let err = renderer
        .fetch(
            "https://example.com/dead",
            &HashMap::new(),
            None,
            deadline(),
        )
        .await
        .expect_err("a navigate failure with the tab still blank is a real failure");
    assert!(err.to_string().contains("navigate returned 500"), "{err}");
}
