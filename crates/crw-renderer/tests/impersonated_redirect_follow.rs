#![cfg(feature = "impersonated")]
//! A same-host redirect within the safe-redirect policy is followed, and the
//! final URL is reported.
//!
//! Own test binary because it sets the process-global
//! `CRW_ALLOW_LOOPBACK_FOR_TESTS`: both hops target the wiremock server, so
//! loopback must be opted in, and that would race other impersonated-tier
//! tests in the crate's `--lib` binary that depend on the variable being
//! unset.

use std::collections::HashMap;
use std::time::Duration;

use crw_core::Deadline;
use crw_renderer::impersonated::ImpersonatedFetcher;
use crw_renderer::traits::PageFetcher;

#[tokio::test]
async fn wiremock_follows_safe_redirect_and_reports_final_url() {
    // SAFETY: one binary per tests/*.rs, so this process owns its env.
    unsafe { std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1") };

    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/start"))
        .respond_with(
            wiremock::ResponseTemplate::new(302)
                .insert_header("location", format!("{}/final", server.uri())),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/final"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string(
                    "<html><body><p>A perfectly ordinary page with more than eighty \
                     characters of visible text, so the accept gate has something to \
                     work with here.</p></body></html>",
                ),
        )
        .mount(&server)
        .await;

    let f = ImpersonatedFetcher::new(None, Duration::from_secs(10)).unwrap();
    let r = f
        .fetch(
            &format!("{}/start", server.uri()),
            &HashMap::new(),
            None,
            Deadline::from_request_ms(10_000),
        )
        .await
        .unwrap();

    assert_eq!(r.status_code, 200);
    assert_eq!(
        r.final_url.as_deref(),
        Some(format!("{}/final", server.uri()).as_str())
    );
}
