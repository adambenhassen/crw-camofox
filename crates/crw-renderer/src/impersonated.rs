//! Chrome-impersonation HTTP tier: a wreq-based fetcher that presents a real
//! Chrome TLS/JA3/HTTP2 fingerprint with no browser and no JS execution.
//!
//! Two roles, both wired in `lib.rs`:
//!
//! * the auto-chain hop between the plain reqwest HTTP tier and the JS
//!   ladder, fired only when the plain tier hit a wall-shaped response (the
//!   Amazon interstitial class) or a transport error, never on SPA/thin/empty
//!   shapes that need JS;
//! * the explicit `renderer = "impersonated-http"` pin.
//!
//! Whole module is `#[cfg(feature = "impersonated")]`: a lean build never
//! links the wreq/BoringSSL stack.

use async_trait::async_trait;
use crw_core::Deadline;
use crw_core::error::{CrwError, CrwResult};
use crw_core::types::FetchResult;
use std::collections::HashMap;
use std::time::Instant;

use crate::http_only;
use crate::traits::PageFetcher;

/// The wreq preset this build impersonates. Pinned, not
/// `wreq_util::Profile::default()` (that is Chrome100, too old for modern
/// walls). Bump deliberately when wreq ships newer presets AND a live wall
/// verifies the newer one still passes (see the `#[ignore]` live tests at the
/// bottom of this file). Not a config knob on purpose: only the Chrome family
/// is verified against live walls.
const CHROME_PRESET: wreq_util::Profile = wreq_util::Emulation::Chrome149;

/// Chrome-impersonating HTTP fetcher. Never executes JS.
pub struct ImpersonatedFetcher {
    client: wreq::Client,
}

impl ImpersonatedFetcher {
    /// Build with the pinned Chrome preset, egressing through `proxy` when one
    /// is configured. **Hard error on build failure**, mirroring the plain
    /// HTTP tier's contract: never fall back to a non-impersonating client,
    /// and never a silent direct connection on a bad proxy URL.
    pub fn new(proxy: Option<&str>, timeout: std::time::Duration) -> CrwResult<Self> {
        let mut builder = wreq::Client::builder()
            // Owns UA + sec-ch-* + accept-encoding so the header set stays
            // coherent with the emulated JA3. The configured CRW user_agent is
            // deliberately NOT injected here.
            .emulation(CHROME_PRESET)
            .connect_timeout(http_only::HTTP_CONNECT_TIMEOUT)
            .timeout(timeout)
            .redirect(safe_redirect_policy_wreq());
        if let Some(proxy_url) = proxy.filter(|p| !p.trim().is_empty()) {
            let p = wreq::Proxy::all(proxy_url).map_err(|_| {
                // NEVER interpolate `proxy_url` itself: it carries `user:pass@`.
                let redacted = crw_core::redact_proxy_url(proxy_url);
                CrwError::ConfigError(format!(
                    "invalid proxy URL '{redacted}' for the impersonated tier"
                ))
            })?;
            builder = builder.proxy(p);
        }
        let client = builder
            .build()
            .map_err(|e| CrwError::ConfigError(format!("impersonated client: {e}")))?;
        Ok(Self { client })
    }
}

/// wreq twin of `crw_core::url_safety::safe_redirect_policy`. The SSRF
/// invariant has ONE owner: `validate_safe_url_blocking_resolved` in crw-core,
/// which both policies call. Only the transport adapter lives here.
///
/// The hop cap mirrors the reqwest twin's `previous().len() >= 10` check.
fn safe_redirect_policy_wreq() -> wreq::redirect::Policy {
    wreq::redirect::Policy::custom(|attempt| {
        if attempt.previous.len() >= 10 {
            attempt.error("too many redirects")
        } else {
            // `http::Uri` has no `as_str()`; Display renders scheme +
            // authority + path, which is what `url::Url::parse` needs.
            match url::Url::parse(&attempt.uri.to_string()) {
                Ok(u) => match crw_core::url_safety::validate_safe_url_blocking_resolved(&u) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(format!("redirect blocked: {e}")),
                },
                Err(e) => attempt.error(format!("invalid redirect target: {e}")),
            }
        }
    })
}

#[async_trait]
impl PageFetcher for ImpersonatedFetcher {
    async fn fetch(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
        _wait_for_ms: Option<u64>,
        deadline: Deadline,
    ) -> CrwResult<FetchResult> {
        if deadline.expired() {
            return Err(CrwError::HttpError(format!(
                "deadline expired before impersonated-HTTP fetch of {url}"
            )));
        }
        let start = Instant::now();

        // Caller-supplied headers override the preset's exactly like the
        // http tier's precedence (per-request beats client default): auth,
        // cookies and the like still apply. Fingerprint-relevant headers are
        // DROPPED, not overridden: the emulation preset owns UA + sec-ch-* +
        // accept-encoding, and a caller UA over a Chrome149 JA3 is precisely
        // the incoherence TLS-fingerprint walls test for.
        let mut req = self.client.get(url);
        for (k, v) in headers {
            let lower = k.to_ascii_lowercase();
            let fingerprint_owned =
                lower == "user-agent" || lower == "accept-encoding" || lower.starts_with("sec-ch-");
            if !fingerprint_owned {
                req = req.header(k.as_str(), v.as_str());
            }
        }

        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Err(CrwError::Timeout(deadline.requested_ms()));
        }
        let resp = match tokio::time::timeout(remaining, req.send()).await {
            Ok(r) => r.map_err(|e| CrwError::HttpError(format!("{url}: {e}")))?,
            Err(_) => return Err(CrwError::Timeout(deadline.requested_ms())),
        };
        let status = resp.status().as_u16();

        if let Some(len) = resp.content_length()
            && len as usize > http_only::MAX_RESPONSE_BYTES
        {
            return Err(CrwError::HttpError(format!(
                "Response too large: {len} bytes (max {})",
                http_only::MAX_RESPONSE_BYTES
            )));
        }
        let content_type_header = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Same challenge-header read the plain HTTP tier stamps into
        // `warning`: reqwest's `HeaderMap` is a re-export of `http::HeaderMap`,
        // which is what wreq returns, so the helper works unchanged.
        let challenge = http_only::challenge_header(resp.headers());

        let final_url_str = resp.uri().to_string();

        // Bound the body read by the caller's remaining budget, floored at
        // `MIN_TIER_BUDGET` for the same reason as http_only: send() resolves
        // on headers and a slow-TTFB origin deserves its last sliver.
        let bytes = match tokio::time::timeout(
            deadline.remaining().max(crate::MIN_TIER_BUDGET),
            resp.bytes(),
        )
        .await
        {
            Ok(r) => r.map_err(|e| CrwError::HttpError(format!("{url}: {e}")))?,
            Err(_) => return Err(CrwError::Timeout(deadline.requested_ms())),
        };

        // Shared response tail with the plain tier: the Chrome preset
        // advertises an Accept-Encoding the client decompresses itself, so
        // the bytes handed over are already plain.
        http_only::build_http_fetch_result(
            url,
            status,
            content_type_header.as_deref(),
            challenge,
            &final_url_str,
            &bytes,
            start.elapsed().as_millis() as u64,
            "impersonated-http",
        )
    }

    fn name(&self) -> &str {
        "impersonated-http"
    }

    fn supports_js(&self) -> bool {
        false
    }

    async fn is_available(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crw_core::Deadline;

    fn fetcher() -> CrwResult<ImpersonatedFetcher> {
        ImpersonatedFetcher::new(None, std::time::Duration::from_secs(10))
    }

    #[test]
    fn client_builds_with_chrome_preset() {
        assert!(fetcher().is_ok());
    }

    #[test]
    fn client_build_is_fail_closed_with_bad_proxy() {
        let f =
            ImpersonatedFetcher::new(Some("not a valid uri"), std::time::Duration::from_secs(10));
        assert!(f.is_err(), "a malformed proxy must be a hard error");
    }

    #[test]
    fn name_and_js_support() {
        let f = fetcher().unwrap();
        assert_eq!(f.name(), "impersonated-http");
        assert!(!f.supports_js());
    }

    /// A clean 200 is accepted with the right `rendered_with`, no
    /// `final_url`, and no challenge warning.
    #[tokio::test]
    async fn wiremock_clean_200_is_accepted() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/page"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .set_body_string(
                        "<html><body><h1>Nene Toys store page</h1>\
                         <p>Wooden fire truck with magnetic tubes, hydrant and a 360 degree \
                         swivel scale, educational toy for kids.</p></body></html>",
                    ),
            )
            .mount(&server)
            .await;
        let f = fetcher().unwrap();
        let r = f
            .fetch(
                &format!("{}/page", server.uri()),
                &HashMap::new(),
                None,
                Deadline::from_request_ms(10_000),
            )
            .await
            .unwrap();
        assert_eq!(r.rendered_with.as_deref(), Some("impersonated-http"));
        assert_eq!(r.status_code, 200);
        assert!(r.html.contains("Nene Toys"));
        assert!(r.warning.is_none());
        assert!(r.warnings.is_empty());
        assert!(r.final_url.is_none());
    }

    /// An interstitial-shaped 200 comes back as a plain FetchResult; the
    /// ACCEPT decision belongs to the caller's gate, not to the fetcher.
    #[tokio::test]
    async fn wiremock_interstitial_body_is_returned_verbatim() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string(
                        "<html><body>Fai clic sul pulsante qui sotto per continuare \
                         a fare acquisti</body></html>",
                    ),
            )
            .mount(&server)
            .await;
        let f = fetcher().unwrap();
        let r = f
            .fetch(
                &server.uri(),
                &HashMap::new(),
                None,
                Deadline::from_request_ms(10_000),
            )
            .await
            .unwrap();
        assert!(r.html.contains("fare acquisti"));
        assert!(r.warning.is_none());
    }

    /// SSRF: a redirect whose target violates the shared URL policy is
    /// blocked. The fixture uses an over-length target (2048+ chars) because
    /// that check runs BEFORE the loopback escape hatch, so the outcome does
    /// not depend on the env var. Proves the wreq policy really calls the
    /// `validate_safe_url_blocking_resolved` owner.
    #[tokio::test]
    async fn wiremock_ssrf_redirect_is_blocked() {
        let server = wiremock::MockServer::start().await;
        let oversized_target = format!("http://example.com/{}", "a".repeat(2100));
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302).insert_header("location", oversized_target),
            )
            .mount(&server)
            .await;
        let f = fetcher().unwrap();
        let r = f
            .fetch(
                &server.uri(),
                &HashMap::new(),
                None,
                Deadline::from_request_ms(10_000),
            )
            .await;
        match r {
            Err(CrwError::HttpError(msg)) => {
                assert!(
                    msg.contains("redirect blocked"),
                    "expected the SSRF policy to block, got: {msg}"
                );
            }
            other => panic!("expected redirect-blocked error, got: {other:?}"),
        }
    }

    /// Challenge-header stamping: a `cf-mitigated: challenge` response
    /// carries the same `warning` marker the auto arm matches on.
    #[tokio::test]
    async fn wiremock_challenge_header_is_stamped_into_warning() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(403)
                    .insert_header("cf-mitigated", "challenge")
                    .insert_header("content-type", "text/html")
                    .set_body_string("<html>challenge</html>"),
            )
            .mount(&server)
            .await;
        let f = fetcher().unwrap();
        let r = f
            .fetch(
                &server.uri(),
                &HashMap::new(),
                None,
                Deadline::from_request_ms(10_000),
            )
            .await
            .unwrap();
        assert_eq!(r.warning.as_deref(), Some("cloudflare_mitigated"));
        assert_eq!(r.status_code, 403);
    }

    /// Fingerprint-relevant caller headers are DROPPED; other headers are
    /// kept. The mock only answers a Chrome-looking UA, so the fetch
    /// succeeding proves the preset's UA survived and the caller's did not.
    #[tokio::test]
    async fn wiremock_drops_fingerprint_relevant_caller_headers() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::header_regex(
                "user-agent",
                r"Chrome/\d+",
            ))
            .and(wiremock::matchers::header("x-custom-header", "kept"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string(
                        "<html><body><p>A perfectly ordinary page with more than fifty \
                         characters of visible text, so the accept gate has something \
                         to work with.</p></body></html>",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let f = fetcher().unwrap();
        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), "my-scraper/0.1".to_string());
        headers.insert("X-Custom-Header".to_string(), "kept".to_string());
        let r = f
            .fetch(
                &server.uri(),
                &headers,
                None,
                Deadline::from_request_ms(10_000),
            )
            .await
            .unwrap();
        assert_eq!(r.status_code, 200);
        server.verify().await;
    }

    // ── Live network verification (#[ignore], run explicitly) ───────────

    /// The tier's reason to exist: the Amazon product page must come back
    /// with the real title, not the TLS-fingerprint interstitial. When Amazon
    /// rotates its wall past Chrome149, this is the test that goes red.
    #[tokio::test]
    #[ignore]
    async fn live_amazon_it_product_page_serves_real_content() {
        let f = fetcher().unwrap();
        let r = f
            .fetch(
                "https://www.amazon.it/dp/B0FHQGLXBP",
                &HashMap::new(),
                None,
                Deadline::from_request_ms(30_000),
            )
            .await
            .unwrap();
        assert_eq!(r.rendered_with.as_deref(), Some("impersonated-http"));
        let lower = r.html.to_lowercase();
        assert!(
            lower.contains("nene toys"),
            "expected the product title in the body"
        );
        assert!(
            !lower.contains("fai clic sul pulsante qui sotto"),
            "got the interstitial, not the product page"
        );
    }
}
