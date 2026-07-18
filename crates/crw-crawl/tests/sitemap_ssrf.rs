//! SSRF guard for sitemap fetches — in its OWN test binary on purpose: the
//! guard's test escape (`CRW_ALLOW_LOOPBACK_FOR_TESTS`) is process-global env,
//! and every wiremock-based sibling sets it. This binary must never set it.

use std::time::Instant;

/// A private-range sitemap URL must be rejected by the resolve-and-validate
/// guard BEFORE any GET is attempted. The empty result must come back fast —
/// if the guard were gone, the client would try to connect to the private
/// address and burn the 15s per-fetch timeout.
#[tokio::test]
async fn private_host_sitemap_is_blocked_before_fetch() {
    let client = reqwest::Client::new();
    for url in [
        "http://169.254.169.254/sitemap.xml",
        "http://10.255.255.1/sitemap.xml",
        "http://127.0.0.1:9/sitemap.xml",
    ] {
        let started = Instant::now();
        let r = crw_crawl::sitemap::fetch_sitemap(url, &client)
            .await
            .expect("guard returns empty, not error");
        assert!(r.is_empty(), "{url} must yield no URLs");
        assert!(
            started.elapsed().as_secs() < 5,
            "{url} must be blocked pre-GET, not via connect timeout"
        );
    }
}
