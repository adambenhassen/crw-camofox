//! A host latched to the proxy egress must not get a cached `cf_clearance`:
//! the cookie is bound to the direct egress IP. Own test binary because the
//! egress latch is process-global.

use std::collections::HashMap;

use crw_core::Deadline;
use crw_core::config::{RendererConfig, RendererMode, StealthConfig};
use crw_renderer::FallbackRenderer;
use crw_renderer::clearance::{Clearance, Cookie};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn no_injection_for_an_egress_latched_host() {
    // SAFETY: one binary per tests/*.rs, so this process owns its env.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") }
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<html><body>real page with enough text to count as content here</body></html>",
        ))
        .mount(&origin)
        .await;
    let url = origin.uri();
    let host = url::Url::parse(&url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_owned();

    let cfg = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    let r = FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default()).unwrap();
    let clearance = Clearance::from_browser(
        vec![Cookie {
            name: "cf_clearance".into(),
            value: "abc123".into(),
            domain: host.clone(),
            path: "/".into(),
            expires: -1.0,
        }],
        "Firefox/128.0".into(),
        0.0,
    )
    .unwrap();
    r.clearance().insert(&host, clearance).await;
    crw_renderer::egress::global().note_block(&host).await;
    assert!(crw_renderer::egress::global().should_proxy(&host).await);

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

    for req in origin.received_requests().await.unwrap() {
        assert!(
            req.headers.get("cookie").is_none(),
            "a latched host must not receive the cached clearance"
        );
    }
}
