//! Clearance reuse on the HTTP tier: a cached `cf_clearance` + UA for a host
//! is injected into the HTTP-tier fetch, dropped when the origin still
//! challenges, and never used when a proxy or caller headers are in play.
//!
//! Own test binary: sets `CRW_ALLOW_LOOPBACK_FOR_TESTS` for this process.

use std::collections::HashMap;

use crw_core::Deadline;
use crw_core::config::{RendererConfig, RendererMode, StealthConfig};
use crw_renderer::FallbackRenderer;
use crw_renderer::clearance::{Clearance, Cookie};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIREFOX_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
const CHALLENGE_HTML: &str = "<html><head><title>Just a moment...</title></head><body><div id=\"challenge-running\"></div><script src=\"/cdn-cgi/challenge-platform/h/b/orchestrate/x.js\"></script></body></html>";

fn allow_loopback() {
    // SAFETY: one binary per tests/*.rs, so this process owns its env.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") }
}

fn renderer(proxy: Option<&str>) -> FallbackRenderer {
    let cfg = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    FallbackRenderer::new(&cfg, "crw-test", proxy, &StealthConfig::default())
        .expect("renderer builds in http-only mode")
}

fn clearance_for(host: &str) -> Clearance {
    Clearance::from_browser(
        vec![Cookie {
            name: "cf_clearance".into(),
            value: "abc123".into(),
            domain: host.into(),
            path: "/".into(),
            expires: -1.0,
        }],
        FIREFOX_UA.into(),
        0.0,
    )
    .unwrap()
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).unwrap().host_str().unwrap().to_owned()
}

async fn origin(body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn http_tier_receives_cached_cookie_and_ua() {
    allow_loopback();
    let origin =
        origin("<html><body>real page with enough text to count as content here</body></html>")
            .await;
    let url = origin.uri();
    let r = renderer(None);
    r.clearance()
        .insert(&host_of(&url), clearance_for(&host_of(&url)))
        .await;

    r.fetch(
        &url,
        &HashMap::new(),
        Some(false),
        None,
        None,
        Deadline::from_request_ms(8_000),
    )
    .await
    .expect("fetch ok");

    let req = origin.received_requests().await.unwrap().pop().unwrap();
    assert_eq!(
        req.headers.get("cookie").unwrap().to_str().unwrap(),
        "cf_clearance=abc123"
    );
    assert_eq!(
        req.headers.get("user-agent").unwrap().to_str().unwrap(),
        FIREFOX_UA
    );
    assert!(req.headers.get("sec-ch-ua").is_none());
    assert!(
        r.clearance().get(&host_of(&url)).await.is_some(),
        "entry kept on success"
    );
}

#[tokio::test]
async fn clearance_invalidated_on_repeat_challenge() {
    allow_loopback();
    let origin = origin(CHALLENGE_HTML).await;
    let url = origin.uri();
    let r = renderer(None);
    r.clearance()
        .insert(&host_of(&url), clearance_for(&host_of(&url)))
        .await;

    let _ = r
        .fetch(
            &url,
            &HashMap::new(),
            Some(false),
            None,
            None,
            Deadline::from_request_ms(8_000),
        )
        .await;

    assert!(
        r.clearance().get(&host_of(&url)).await.is_none(),
        "rejected clearance must be dropped"
    );
}

#[tokio::test]
async fn no_injection_when_http_tier_has_a_proxy() {
    allow_loopback();
    // The "proxy" is a wiremock that answers any absolute-URI GET.
    let proxy =
        origin("<html><body>via proxy, plenty of body text to be content</body></html>").await;
    let origin = origin("<html>never reached</html>").await;
    let url = origin.uri();
    let r = renderer(Some(&proxy.uri()));
    r.clearance()
        .insert(&host_of(&url), clearance_for(&host_of(&url)))
        .await;

    r.fetch(
        &url,
        &HashMap::new(),
        Some(false),
        None,
        None,
        Deadline::from_request_ms(8_000),
    )
    .await
    .expect("fetch via proxy ok");

    let req = proxy
        .received_requests()
        .await
        .unwrap()
        .pop()
        .expect("proxy saw the request");
    assert!(
        req.headers.get("cookie").is_none(),
        "cf_clearance is IP-bound; never send it through a proxy"
    );
    assert_ne!(
        req.headers.get("user-agent").unwrap().to_str().unwrap(),
        FIREFOX_UA
    );
}

#[tokio::test]
async fn caller_cookie_header_wins() {
    allow_loopback();
    let origin =
        origin("<html><body>real page with enough text to count as content here</body></html>")
            .await;
    let url = origin.uri();
    let r = renderer(None);
    r.clearance()
        .insert(&host_of(&url), clearance_for(&host_of(&url)))
        .await;

    let mut headers = HashMap::new();
    headers.insert("Cookie".to_string(), "mine=1".to_string());
    r.fetch(
        &url,
        &headers,
        Some(false),
        None,
        None,
        Deadline::from_request_ms(8_000),
    )
    .await
    .expect("fetch ok");

    let req = origin.received_requests().await.unwrap().pop().unwrap();
    assert_eq!(
        req.headers.get("cookie").unwrap().to_str().unwrap(),
        "mine=1"
    );
    assert_ne!(
        req.headers.get("user-agent").unwrap().to_str().unwrap(),
        FIREFOX_UA,
        "no partial injection"
    );
}
