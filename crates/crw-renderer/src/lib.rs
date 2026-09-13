//! HTTP and headless-browser rendering engine for the CRW web scraper.
//!
//! Provides a [`FallbackRenderer`] that fetches pages via plain HTTP and optionally
//! re-renders them through a CDP-based headless browser when SPA content is detected.
//!
//! - [`http_only`] — Simple HTTP fetcher using `reqwest`
//! - [`detector`] — Heuristic SPA shell detection (empty body, framework markers)
//! - `cdp` — Chrome DevTools Protocol renderer (LightPanda, Playwright, Chrome) *(requires `cdp` feature)*
//! - [`traits`] — [`PageFetcher`] trait for pluggable backends
//!
//! # Feature flags
//!
//! | Flag  | Description |
//! |-------|-------------|
//! | `cdp` | Enables CDP WebSocket rendering via `tokio-tungstenite` |
//!
//! # Example
//!
//! ```rust,no_run
//! use crw_core::config::RendererConfig;
//! use crw_renderer::FallbackRenderer;
//! use std::collections::HashMap;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use crw_core::config::StealthConfig;
//! let config = RendererConfig::default();
//! let stealth = StealthConfig::default();
//! let renderer = FallbackRenderer::new(&config, "crw/0.1", None, &stealth)?;
//! let deadline = crw_core::Deadline::from_request_ms(8000);
//! let result = renderer.fetch("https://example.com", &HashMap::new(), None, None, None, deadline).await?;
//! println!("status: {}", result.status_code);
//! # Ok(())
//! # }
//! ```

pub mod blocklist;
pub mod breaker;
#[cfg(feature = "auto-browser")]
pub mod browser;
#[cfg(feature = "cdp")]
pub mod browser_pool;
#[cfg(feature = "camofox")]
pub mod camofox;
#[cfg(feature = "cdp")]
pub mod cdp;
#[cfg(feature = "cdp")]
pub mod cdp_conn;
pub mod detector;
pub mod egress;
#[cfg(feature = "cdp")]
pub mod health_telemetry;
pub mod host_limiter;
pub mod http_only;
pub mod preference;
pub mod traits;

use crate::breaker::{
    AttemptContext, BreakerOutcome, BreakerRegistry, Permit, ProbeGuard, classify_outcome,
};
use crate::preference::HostPreferences;
use crw_core::config::{BUILTIN_UA_POOL, RendererConfig, RendererMode, StealthConfig};
use crw_core::error::{CrwError, CrwResult};
use crw_core::metrics::metrics;
use crw_core::types::{
    FailoverErrorKind, FetchResult, RenderDecision, RendererKind, resolve_render_js,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use traits::PageFetcher;

tokio::task_local! {
    /// Per-request country code (ISO 3166-1 alpha-2, lowercase) for the
    /// chrome_proxy tier's CDP auth pump. Set by `FallbackRenderer::fetch`
    /// when a `ScrapeRequest.country` is present; read in `cdp.rs` while
    /// composing DataImpulse credentials. Task-local so child tasks
    /// spawned by the pool inherit it without trait-signature churn.
    pub static REQUEST_COUNTRY: Option<String>;
}

/// Map a renderer's name string to the closed `RendererKind` enum.
/// Returns `None` for unknown names (e.g. "playwright" — treated as a
/// JS renderer but not tracked in metrics/preferences).
fn renderer_kind_for(name: &str) -> Option<RendererKind> {
    match name {
        "http" | "http_only_fallback" => Some(RendererKind::Http),
        "lightpanda" => Some(RendererKind::Lightpanda),
        "chrome" => Some(RendererKind::Chrome),
        "chrome_proxy" => Some(RendererKind::ChromeProxy),
        "camofox" => Some(RendererKind::Camofox),
        _ => None,
    }
}

/// Classify a renderer-side error into a `FailoverErrorKind` for the
/// preference learner. Match on `CrwError` variants (not error strings),
/// so renaming or rewording the human-readable message can't silently
/// reclassify failures and over-promote hosts.
///
/// Only LightPanda-specific failures drive promotion (see
/// [`FailoverErrorKind::counts_for_promotion`]); transport / unreachable
/// errors stay in `NetworkError` so a flaky upstream doesn't push hosts
/// to Chrome.
fn classify_renderer_error(err: &CrwError) -> FailoverErrorKind {
    match err {
        CrwError::Timeout(_) => FailoverErrorKind::LightpandaTimeout,
        CrwError::TargetUnreachable(_) => FailoverErrorKind::NetworkError,
        CrwError::HttpError(_) => FailoverErrorKind::NetworkError,
        // RendererError covers WS disconnects, CDP frame errors, render
        // pipeline crashes — these are LightPanda-attributable.
        CrwError::RendererError(_) => FailoverErrorKind::LightpandaCrash,
        _ => FailoverErrorKind::Other,
    }
}

/// Build a per-tier timeout map from the renderer config. Used by the
/// breaker layer for pre-flight skip and clamp detection.
fn tier_timeouts_from(
    config: &RendererConfig,
) -> std::collections::HashMap<RendererKind, std::time::Duration> {
    let mut m = std::collections::HashMap::new();
    m.insert(
        RendererKind::Http,
        std::time::Duration::from_millis(config.http_timeout()),
    );
    m.insert(
        RendererKind::Lightpanda,
        std::time::Duration::from_millis(config.lightpanda_timeout()),
    );
    m.insert(
        RendererKind::Chrome,
        std::time::Duration::from_millis(config.chrome_timeout()),
    );
    m.insert(
        RendererKind::ChromeProxy,
        std::time::Duration::from_millis(config.chrome_proxy_timeout()),
    );
    m
}

/// Per-renderer credit cost. Exposed so the routing layer can populate
/// `FetchResult.credit_cost` and `/v1/scrape` charge accurately.
fn credit_for(kind: RendererKind) -> u32 {
    match kind {
        RendererKind::Http => 1,
        RendererKind::Lightpanda => 1,
        RendererKind::Chrome => 2,
        // Engine-internal cost only. SaaS billing reads request-body
        // `renderer` string and still charges 1 credit per scrape regardless.
        RendererKind::ChromeProxy => 2,
        // Camofox is the heavy/stealth tier — same internal cost as Chrome.
        RendererKind::Camofox => 2,
    }
}

/// Stamp `render_decision` and `credit_cost` for an HTTP-only result.
/// `requested_renderer` is taken into account: if the user explicitly
/// pinned `"http"` we mark it as `UserPinned`, otherwise `AutoDefault`.
fn stamp_http_decision(result: &mut FetchResult, requested_renderer: Option<&str>) {
    if result.render_decision.is_some() {
        return;
    }
    let kind = RendererKind::Http;
    result.credit_cost = credit_for(kind);
    result.render_decision = Some(match requested_renderer {
        Some("http") => RenderDecision::UserPinned { renderer: kind },
        _ => RenderDecision::AutoDefault { chosen: kind },
    });
    // Mirror the JS-renderer metric so dashboards see HTTP routing too.
    metrics()
        .render_route_decision_total
        .with_label_values(&[kind.as_str(), "success"])
        .inc();
}

/// Extract the host from a URL string, returning an empty string on failure.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default()
}

/// Pick a user-agent: rotate from stealth pool when stealth is enabled.
fn pick_ua<'a>(default_ua: &'a str, stealth: &'a StealthConfig) -> String {
    if stealth.enabled {
        let pool: &[&str] = if stealth.user_agents.is_empty() {
            BUILTIN_UA_POOL
        } else {
            // Safe: user_agents is non-empty in this branch.
            return stealth.user_agents[rand::random_range(0..stealth.user_agents.len())].clone();
        };
        pool[rand::random_range(0..pool.len())].to_string()
    } else {
        default_ua.to_string()
    }
}

/// Did this renderer error come from failing to reach/navigate the ORIGIN, as
/// opposed to a fault on our side (CDP pool exhausted, browser discovery failed,
/// a pinned renderer that does not exist)?
///
/// Only used to decide whether an unreachable origin should outrank a JS-tier error
/// when both fail. Getting it wrong in the permissive direction (treating our fault as
/// the origin's) would blame the caller for our outage, so the match is deliberately
/// narrow.
///
/// `Timeout` is NOT included: a JS-tier timeout is just as likely to be a local CDP
/// websocket/command timeout as a slow origin, and it already maps to 504 on its own,
/// which is the honest answer either way.
fn is_origin_navigation_failure(e: &CrwError) -> bool {
    match e {
        CrwError::TargetUnreachable(_) => true,
        // LightPanda/camofox report a failure to reach the origin as a navigation
        // error with a `net::ERR_*` code. Internal faults (pool exhausted, CDP
        // discovery) carry different messages and keep their own error.
        CrwError::RendererError(msg) => {
            let m = msg.to_ascii_lowercase();
            m.contains("navigation failed") || m.contains("net::err_")
        }
        _ => false,
    }
}

/// Is this failure the ORIGIN's fault, for breaker-scoping purposes only?
///
/// Deliberately narrower than [`is_origin_navigation_failure`], which decides
/// error ATTRIBUTION and is generous about `net::ERR_*` on purpose. The breaker
/// asks whether the failure says anything about the TIER's own health. A proxy
/// tunnel that will not open, and a box that lost its network, share the
/// `net::ERR_` shape of a dead origin, but those are ours and must keep reaching
/// the global window.
fn is_origin_fault_for_breaker(e: &CrwError) -> bool {
    if let CrwError::RendererError(m) = e {
        let u = m.to_ascii_uppercase();
        if u.contains("ERR_TUNNEL_CONNECTION_FAILED")
            || u.contains("ERR_PROXY_CONNECTION_FAILED")
            || u.contains("ERR_NETWORK_CHANGED")
            || u.contains("ERR_INTERNET_DISCONNECTED")
        {
            return false;
        }
    }
    is_origin_navigation_failure(e)
}

/// Prefix of the `warning` set when a JS escalation failed and the HTTP body was
/// returned in its place. Public because it is BOTH the caller-facing
/// explanation and the signal `crw_crawl::single` reads to skip a second
/// escalation round that would re-run a ladder this request already exhausted.
/// A shared constant so the producer and that consumer cannot drift.
pub const JS_ESCALATION_FAILED: &str = "js_escalation_failed:";

/// Soft-block / soft-error status codes where the body often contains real
/// content despite the status header. Sources:
///   - UA/header-based bot filters: 401, 403, 405, 406, 412
///   - Rate limits: 429
///   - Geo gates: 451
///   - Origin overload: 503
///   - "Not found" SPAs that 404 the route but render content via JS
///     hydration: 404, 410
///   - Origin error that still serves a usable page: 500
///
/// Firecrawl-comparison (April 2026 bench): the JS render path recovered
/// content in ~25/99 such cases that HTTP alone could not. Shared by the auto
/// and forced-JS arms of `fetch`, which must agree on what counts as a body
/// worth warning about.
fn is_soft_block_status(status_code: u16) -> bool {
    matches!(
        status_code,
        401 | 403 | 404 | 405 | 406 | 410 | 412 | 429 | 451 | 500 | 503
    )
}

/// Minimum remaining request budget for a network attempt to be worth making.
/// Below this a CDP tier cannot complete its handshake and returns a fabricated
/// `Timeout after Nms` (single-digit N) while still consuming a pool slot.
/// Guards the main ladder loop, the breaker leak-through arm, and the HTTP
/// tier's proxy retry (`http_only`).
pub(crate) const MIN_TIER_BUDGET: Duration = Duration::from_millis(500);

/// Composite renderer that tries multiple backends in order.
pub struct FallbackRenderer {
    http: Arc<dyn PageFetcher>,
    js_renderers: Vec<Arc<dyn PageFetcher>>,
    /// Global default for `render_js` when a request doesn't specify one.
    render_js_default: Option<bool>,
    /// Per-host renderer preference learning (auto-mode only).
    preferences: Arc<HostPreferences>,
    /// Per-host + global circuit breakers per renderer.
    breakers: Arc<BreakerRegistry>,
    /// Per-tier configured timeouts (Duration). Used by the breaker layer
    /// for pre-flight deadline-skip and clamp detection in
    /// `AttemptContext::capture`.
    tier_timeouts: std::collections::HashMap<RendererKind, std::time::Duration>,
    /// Process-wide per-eTLD+1 rate (req/sec). `0.0` disables the interval
    /// floor; the concurrency cap below still applies. Configured via
    /// [`Self::with_host_limits`].
    requests_per_second: f64,
    /// Process-wide per-eTLD+1 in-flight cap. `1` enforces strict politeness.
    per_host_max_concurrent: u32,
    /// Anti-bot classifier policy. Drives the in-loop `classify()` call that
    /// decides whether a 200-status block page is a soft failure (escalate
    /// toward `chrome_proxy`) or a genuine success.
    antibot: crw_core::config::AntibotConfig,
    /// Chrome browser-context pool handle for graceful drain on shutdown.
    /// `None` when the pool is disabled or the chrome tier isn't configured.
    #[cfg(feature = "cdp")]
    chrome_pool: Option<Arc<browser_pool::BrowserContextPool<cdp_conn::CdpConnection>>>,
}

impl std::fmt::Debug for FallbackRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FallbackRenderer")
            .field("http", &self.http.name())
            .field(
                "js_renderers",
                &self
                    .js_renderers
                    .iter()
                    .map(|r| r.name())
                    .collect::<Vec<_>>(),
            )
            .field("render_js_default", &self.render_js_default)
            .finish()
    }
}

impl FallbackRenderer {
    pub fn new(
        config: &RendererConfig,
        user_agent: &str,
        proxy: Option<&str>,
        stealth: &StealthConfig,
    ) -> CrwResult<Self> {
        let effective_ua = pick_ua(user_agent, stealth);
        let inject_headers = stealth.enabled && stealth.inject_headers;
        let http = Arc::new(http_only::HttpFetcher::with_timeout(
            &effective_ua,
            proxy,
            inject_headers,
            std::time::Duration::from_millis(config.http_timeout()),
        )) as Arc<dyn PageFetcher>;

        // A pinned backend (Lightpanda/Chrome/Playwright) must have CDP compiled in
        // AND its matching endpoint configured. `Auto` and `None` remain functional
        // without CDP — they just won't spawn any JS renderer.
        #[cfg(not(feature = "cdp"))]
        if matches!(
            config.mode,
            RendererMode::Lightpanda | RendererMode::Chrome | RendererMode::Playwright
        ) {
            return Err(CrwError::ConfigError(format!(
                "renderer.mode = {:?} requires the 'cdp' feature, but this build was \
                 compiled without it. Rebuild with --features cdp or set mode = \"auto\"/\"none\".",
                config.mode
            )));
        }

        #[allow(unused_mut)]
        let mut js_renderers: Vec<Arc<dyn PageFetcher>> = Vec::new();

        if matches!(config.mode, RendererMode::None) {
            if config.render_js_default == Some(true) {
                tracing::warn!(
                    "render_js_default=true has no effect with mode=none; \
                     requests will fall back to HTTP via http_only_fallback"
                );
            }
            return Ok(Self {
                http,
                js_renderers,
                render_js_default: config.render_js_default,
                preferences: Arc::new(HostPreferences::with_defaults()),
                breakers: Arc::new(BreakerRegistry::with_defaults()),
                tier_timeouts: tier_timeouts_from(config),
                requests_per_second: 0.0,
                per_host_max_concurrent: 1,
                antibot: config.antibot.clone(),
                #[cfg(feature = "cdp")]
                chrome_pool: None,
            });
        }

        #[cfg(feature = "cdp")]
        let chrome_pool: Option<
            Arc<browser_pool::BrowserContextPool<cdp_conn::CdpConnection>>,
        > = None;

        #[cfg(any(feature = "cdp", feature = "camofox"))]
        let want = |m: RendererMode| -> bool {
            matches!(config.mode, RendererMode::Auto) || config.mode == m
        };

        #[cfg(feature = "cdp")]
        {
            if want(RendererMode::Lightpanda) {
                if let Some(lp) = &config.lightpanda {
                    js_renderers.push(Arc::new(
                        cdp::CdpRenderer::new(
                            "lightpanda",
                            &lp.ws_url,
                            config.lightpanda_timeout(),
                            config.pool_size,
                        )
                        .with_user_agent(&effective_ua),
                    ));
                } else if matches!(config.mode, RendererMode::Lightpanda) {
                    return Err(CrwError::ConfigError(
                        "renderer.mode = \"lightpanda\" but [renderer.lightpanda] ws_url is not \
                         configured"
                            .into(),
                    ));
                }
            }
        }

        // Camofox (Firefox via camofox-browser REST) takes Chrome's slot:
        // tried after LightPanda and before the CDP chrome tiers. It is not
        // CDP, so it carries no browser-context pool and does not depend on
        // the `cdp` feature — a camofox-only build must still register it.
        #[cfg(feature = "camofox")]
        if want(RendererMode::Camofox) {
            if let Some(cf) = &config.camofox {
                js_renderers.push(Arc::new(camofox::CamofoxRenderer::new(
                    "camofox",
                    &cf.base_url,
                    cf.api_key.clone(),
                    Duration::from_millis(config.chrome_timeout()),
                )));
            } else if matches!(config.mode, RendererMode::Camofox) {
                return Err(CrwError::ConfigError(
                    "renderer.mode = \"camofox\" but [renderer.camofox] base_url is not \
                     configured"
                        .into(),
                ));
            }
        }
        #[cfg(not(feature = "camofox"))]
        if matches!(config.mode, RendererMode::Camofox) {
            return Err(CrwError::ConfigError(
                "renderer.mode = \"camofox\" but this binary was built without the `camofox` \
                 feature"
                    .into(),
            ));
        }

        // Spawn the process-wide CDP telemetry sampler. Idempotent —
        // OnceLock guarantees a single task across all FallbackRenderer
        // instances. No-op on the `mode = none` early-return path above.
        #[cfg(feature = "cdp")]
        health_telemetry::spawn_once();

        if config.render_js_default == Some(true) && js_renderers.is_empty() {
            tracing::warn!(
                "render_js_default=true but no JS renderer is available; \
                 requests will fall back to HTTP via http_only_fallback"
            );
        }

        Ok(Self {
            http,
            js_renderers,
            render_js_default: config.render_js_default,
            preferences: Arc::new(HostPreferences::with_defaults()),
            breakers: Arc::new(BreakerRegistry::with_defaults()),
            tier_timeouts: tier_timeouts_from(config),
            requests_per_second: 0.0,
            per_host_max_concurrent: 1,
            antibot: config.antibot.clone(),
            #[cfg(feature = "cdp")]
            chrome_pool,
        })
    }

    /// True when a JS renderer (lightpanda / camofox) is wired in, so a
    /// `render_js` request can actually execute a page. The sitemap
    /// escalation arm uses this to skip pointless re-fetches of a challenged
    /// sitemap when no renderer could clear the wall anyway.
    pub fn js_capable(&self) -> bool {
        !self.js_renderers.is_empty()
    }

    /// Drain the chrome browser-context pool. Idempotent and a no-op when
    /// the pool is disabled. Call from the server's SIGTERM handler after
    /// the HTTP server has finished serving in-flight requests.
    #[cfg(feature = "cdp")]
    pub async fn shutdown_chrome_pool(&self, drain: std::time::Duration) {
        if let Some(pool) = self.chrome_pool.clone() {
            tracing::info!(
                drain_secs = drain.as_secs(),
                "draining chrome browser-context pool"
            );
            pool.shutdown(drain).await;
        }
    }

    /// No-op when the `cdp` feature is disabled — keeps caller code simple.
    #[cfg(not(feature = "cdp"))]
    pub async fn shutdown_chrome_pool(&self, _drain: std::time::Duration) {}

    /// Configure the process-wide per-host limiter (eTLD+1 keyed). Call once
    /// at startup with values from `CrawlerConfig`. Defaults: rps=0.0 (no
    /// interval floor), per-host cap=1 (strict politeness).
    pub fn with_host_limits(
        mut self,
        requests_per_second: f64,
        per_host_max_concurrent: u32,
    ) -> Self {
        self.requests_per_second = requests_per_second;
        self.per_host_max_concurrent = per_host_max_concurrent;
        self
    }

    /// Access the host preferences cache (for admin endpoints, tests).
    pub fn preferences(&self) -> Arc<HostPreferences> {
        Arc::clone(&self.preferences)
    }

    /// Access the breaker registry (for tests).
    pub fn breakers(&self) -> Arc<BreakerRegistry> {
        Arc::clone(&self.breakers)
    }

    /// Names of the configured JS renderers in fallback order.
    /// Used for startup logs and tests — does not leak internal types.
    pub fn js_renderer_names(&self) -> Vec<&str> {
        self.js_renderers.iter().map(|r| r.name()).collect()
    }

    /// Which tier a post-LightPanda escalation should aim at, or `None` when
    /// this pool has nothing above lightpanda and the escalation should be
    /// skipped rather than dispatched.
    ///
    /// A pinned name the pool does not hold is a hard error, not a fallback, so
    /// the target must come from the tiers actually constructed. On this fork
    /// that is camofox when configured.
    pub fn lightpanda_escalation_target(&self) -> Option<&str> {
        self.js_renderers
            .iter()
            .map(|r| r.name())
            .find(|name| *name != "lightpanda")
    }

    /// Fetch a URL with smart mode: HTTP first, then JS if needed.
    ///
    /// When `render_js` is `None` (auto-detect), the renderer also escalates to
    /// JS rendering if the HTTP response looks like an anti-bot challenge page
    /// (Cloudflare "Just a moment...", etc.). The CDP renderer has built-in
    /// challenge retry logic that waits for non-interactive JS challenges to
    /// auto-resolve.
    pub async fn fetch(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
        render_js: Option<bool>,
        wait_for_ms: Option<u64>,
        requested_renderer: Option<&str>,
        deadline: crw_core::Deadline,
    ) -> CrwResult<FetchResult> {
        // Per-eTLD+1 rate-limit + concurrency cap. Held across the entire
        // fetch (including any escalation to a JS renderer) so a host that
        // rate-limits HTTP doesn't get hammered by Chrome on retry.
        let host_key = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(crate::preference::normalize_host));
        let _host_permit = if let Some(key) = host_key.as_deref() {
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err(CrwError::Timeout(deadline.requested_ms()));
            }
            match tokio::time::timeout(
                remaining,
                crate::host_limiter::acquire(
                    key,
                    self.requests_per_second,
                    self.per_host_max_concurrent as usize,
                ),
            )
            .await
            {
                Ok(Ok((permit, sleep))) => {
                    if !sleep.is_zero() {
                        let budget = deadline.remaining();
                        if sleep > budget {
                            return Err(CrwError::Timeout(sleep.as_millis().max(1) as u64));
                        }
                        tokio::time::sleep(sleep).await;
                    }
                    Some(permit)
                }
                Ok(Err(_)) => return Err(CrwError::RendererError("host limiter closed".into())),
                Err(_) => {
                    return Err(CrwError::Timeout(deadline.requested_ms()));
                }
            }
        } else {
            None
        };

        let effective = resolve_render_js(render_js, self.render_js_default);
        tracing::debug!(
            url,
            request_render_js = ?render_js,
            default_render_js = ?self.render_js_default,
            effective_render_js = ?effective,
            requested_renderer,
            "FallbackRenderer::fetch dispatching"
        );
        // A non-"auto" pinned renderer is a hard pin — failures must surface.
        let is_hard_pinned = matches!(requested_renderer, Some(name) if name != "auto");
        match effective {
            Some(false) => {
                let mut r = self.http.fetch(url, headers, None, deadline).await?;
                stamp_http_decision(&mut r, requested_renderer);
                Ok(r)
            }
            Some(true) => {
                // Fetch via HTTP first to check content type — PDFs can't be JS-rendered.
                // An HTTP-tier failure is not terminal when a JS renderer exists: the
                // caller asked for a browser render (a pinned renderer implies this
                // branch), so escalate the way auto mode does. Previously a pinned
                // camofox scrape of an origin slower than the HTTP tier's timeout
                // 502'd here without ever reaching camofox.
                let mut http_result = match self.http.fetch(url, headers, None, deadline).await {
                    Ok(r) => r,
                    Err(e) if !self.js_renderers.is_empty() => {
                        return self
                            .escalate_after_http_failure(
                                e,
                                url,
                                headers,
                                wait_for_ms,
                                requested_renderer,
                                deadline,
                            )
                            .await;
                    }
                    Err(e) => return Err(e),
                };
                if http_result.content_type.as_deref() == Some("application/pdf") {
                    stamp_http_decision(&mut http_result, requested_renderer);
                    return Ok(http_result);
                }

                if self.js_renderers.is_empty() {
                    tracing::warn!(
                        url,
                        "JS rendering requested but no renderer available — falling back to HTTP"
                    );
                    let mut result = http_result;
                    result.rendered_with = Some("http_only_fallback".to_string());
                    result.warning = Some("JS rendering was requested but no renderer is available. Content was fetched via HTTP only.".to_string());
                    result.warnings.push(
                        "JS rendering requested but no renderer available; HTTP fallback used"
                            .into(),
                    );
                    stamp_http_decision(&mut result, requested_renderer);
                    Ok(result)
                } else {
                    // The HTTP body was already fetched above for the content-type
                    // check, so when the JS ladder fails there is a valid document
                    // in hand — returning `Err` instead of that body is a straight
                    // recall loss, and the auto arm below has never done it.
                    //
                    // Unlike auto, the fallback here is ALWAYS announced. Auto can
                    // swap silently because the caller expressed no preference; a
                    // `renderJs:true` caller asked for a browser and must be able
                    // to tell they did not get one.
                    let is_auth_blocked = is_soft_block_status(http_result.status_code);
                    let started_at = std::time::Instant::now();
                    match self
                        .fetch_with_js(url, headers, wait_for_ms, requested_renderer, deadline)
                        .await
                    {
                        Ok(js_result) => Ok(js_result),
                        // An explicit renderer pin is a caller contract that
                        // forbids silent substitution: fail closed. So does a
                        // shell that IS the wall the ladder just refused to clear:
                        // returning it hands back exactly what every tier
                        // rejected. A shell that is not a wall still substitutes.
                        Err(e)
                            if is_hard_pinned
                                || detector::looks_like_generic_bot_wall(&http_result.html) =>
                        {
                            Err(e)
                        }
                        Err(e) => {
                            if is_auth_blocked {
                                tracing::error!(
                                    url,
                                    status_code = http_result.status_code,
                                    "JS escalation failed for soft-block status; surfacing HTTP shell with warning: {e}"
                                );
                            } else {
                                tracing::warn!(
                                    "JS rendering failed, falling back to HTTP result: {e}"
                                );
                            }
                            let warning = format!("{JS_ESCALATION_FAILED} {e}");
                            http_result.warning = Some(match http_result.warning.take() {
                                Some(prev) => format!("{warning}; {prev}"),
                                None => warning,
                            });
                            // `elapsed_ms` came from the HTTP fetch alone, so it
                            // would report a few hundred ms for a request that
                            // spent the whole deadline in the ladder.
                            http_result.elapsed_ms = http_result
                                .elapsed_ms
                                .saturating_add(started_at.elapsed().as_millis() as u64);
                            // `stamp_http_decision` records a plain `http` route,
                            // which reads as "no browser was needed". Emit the
                            // real story first so forced-JS fallbacks are separable
                            // from ordinary HTTP traffic in metrics.
                            metrics()
                                .render_route_decision_total
                                .with_label_values(&[
                                    RendererKind::Http.as_str(),
                                    "jsLadderExhausted",
                                ])
                                .inc();
                            stamp_http_decision(&mut http_result, requested_renderer);
                            Ok(http_result)
                        }
                    }
                }
            }
            None => {
                // In auto mode, an HTTP-layer failure (TargetUnreachable, body
                // decode mid-stream, oversize response, transient network) is
                // not terminal: if a JS renderer is available, escalate. Many
                // sites that reject reqwest's TLS/UA fingerprint succeed via a
                // real Chromium navigation. Bench analysis: 10/147 false
                // "unreachable" + 5/147 "http_502" map to this branch.
                let mut result = match self.http.fetch(url, headers, None, deadline).await {
                    Ok(r) => r,
                    Err(e) if !self.js_renderers.is_empty() => {
                        return self
                            .escalate_after_http_failure(
                                e,
                                url,
                                headers,
                                wait_for_ms,
                                requested_renderer,
                                deadline,
                            )
                            .await;
                    }
                    Err(e) => return Err(e),
                };

                // PDFs don't need JS rendering — return immediately.
                if result.content_type.as_deref() == Some("application/pdf") {
                    stamp_http_decision(&mut result, requested_renderer);
                    return Ok(result);
                }

                let needs_js = detector::needs_js_rendering(&result.html);
                let cf_header_signal = result.warning.as_deref() == Some("cloudflare_mitigated");
                let is_generic_bot_wall = detector::looks_like_generic_bot_wall(&result.html);
                let is_blocked = cf_header_signal
                    || detector::looks_like_cloudflare_challenge(&result.html)
                    || is_generic_bot_wall;
                let is_auth_blocked = is_soft_block_status(result.status_code);
                // Post-fetch thin-content trigger: HTTP returned 2xx but the
                // body has effectively no extractable text. Catches sites whose
                // SPA marker we don't recognize (no `id="root"`, no
                // `__next_data__`) yet still return a near-empty HTML shell.
                // Bench analysis showed 23/147 failures fall in this bucket
                // (seattletimes, espn, ionos, huduser, …).
                // Escalate a thin 2xx body ONLY when a browser would plausibly
                // reveal more (executable JS, or a meta-refresh redirect). A
                // script-less static doc (e.g. example.com) is already complete,
                // so a headless render just adds seconds for nothing. The
                // recognized-shell sites this bucket targets (seattletimes, espn,
                // …) all ship script bundles, so they still escalate.
                let is_2xx = (200..300).contains(&result.status_code);
                let is_thin_content = is_2xx
                    && detector::looks_like_thin_html(&result.html)
                    && detector::warrants_browser_retry(&result.html);

                if !self.js_renderers.is_empty()
                    && (needs_js || is_blocked || is_auth_blocked || is_thin_content)
                {
                    if is_auth_blocked {
                        tracing::info!(
                            url,
                            status_code = result.status_code,
                            "HTTP {} received, escalating to JS renderer",
                            result.status_code
                        );
                    } else if is_blocked {
                        tracing::info!(
                            url,
                            "Anti-bot challenge detected in HTTP response, escalating to JS renderer"
                        );
                        if is_generic_bot_wall {
                            tracing::info!(
                                url,
                                "Generic anti-bot interstitial detected, escalating to JS renderer"
                            );
                        }
                    } else if needs_js {
                        tracing::info!(url, "SPA shell detected, retrying with JS renderer");
                    } else {
                        tracing::info!(
                            url,
                            html_len = result.html.len(),
                            "HTTP 2xx but body is thin, escalating to JS renderer"
                        );
                    }
                    match self
                        .fetch_with_js(url, headers, wait_for_ms, requested_renderer, deadline)
                        .await
                    {
                        Ok(js_result) => Ok(js_result),
                        Err(e)
                            if is_hard_pinned
                                || detector::looks_like_generic_bot_wall(&result.html) =>
                        {
                            // Pinned: the caller's contract forbids substitution.
                            // Wall: see the sibling arm — the shell is the very
                            // thing the ladder rejected, so it is not a fallback.
                            Err(e)
                        }
                        Err(e) => {
                            // For `is_auth_blocked` (4xx/5xx soft-block status codes), the
                            // HTTP body is almost certainly an error shell — falling back
                            // to it silently misleads the caller. For `needs_js` /
                            // `is_blocked` / `is_thin_content`, the HTTP body still has
                            // *some* useful content, so the fallback itself stays silent
                            // and only the log level differs.
                            //
                            // The warning tag does NOT differ. `JS_ESCALATION_FAILED` is
                            // how `crw_crawl::single` learns the ladder is spent
                            // (`js_ladder_exhausted`); tagging only the soft-block arm let
                            // a body-detected block on a plain 200 — the canonical
                            // Turnstile-over-200 shape — re-run the ENTIRE ladder against
                            // a site that had just failed it.
                            if is_auth_blocked {
                                tracing::error!(
                                    url,
                                    status_code = result.status_code,
                                    "JS escalation failed for soft-block status; surfacing HTTP shell with warning: {e}"
                                );
                            } else {
                                tracing::warn!(
                                    "JS rendering failed, falling back to HTTP result: {e}"
                                );
                            }
                            let warning = format!("{JS_ESCALATION_FAILED} {e}");
                            result.warning = Some(match result.warning.take() {
                                Some(prev) => format!("{warning}; {prev}"),
                                None => warning,
                            });
                            stamp_http_decision(&mut result, requested_renderer);
                            Ok(result)
                        }
                    }
                } else {
                    stamp_http_decision(&mut result, requested_renderer);
                    Ok(result)
                }
            }
        }
    }

    /// Minimum body text length for a JS-rendered result to be considered
    /// successful. If the rendered page has less visible text than this, the
    /// next renderer in the chain is tried.
    const MIN_RENDERED_TEXT_LEN: usize = 50;

    /// The HTTP tier failed but a JS renderer exists: escalate to it. If the
    /// JS tier fails too, pick the error that names the root cause.
    async fn escalate_after_http_failure(
        &self,
        http_err: CrwError,
        url: &str,
        headers: &HashMap<String, String>,
        wait_for_ms: Option<u64>,
        requested_renderer: Option<&str>,
        deadline: crw_core::Deadline,
    ) -> CrwResult<FetchResult> {
        tracing::info!(
            url,
            error = %http_err,
            "HTTP fetch failed, escalating to JS renderer"
        );
        self.fetch_with_js(url, headers, wait_for_ms, requested_renderer, deadline)
            .await
            .map_err(|js_err| {
                tracing::warn!("Both HTTP and JS failed: http={http_err}, js={js_err}");
                // When the HTTP tier could not reach the origin AND the JS tier
                // failed navigating to that same origin, the origin is the root
                // cause: surface TargetUnreachable (422 — the caller handed us a
                // dead target) instead of the JS tier's RendererError, which
                // falls through to a 500 and reads as "our server broke".
                //
                // A JS failure can also be OUR fault (pool exhausted, CDP
                // discovery failed, pinned renderer missing). Those keep their
                // own error, or we would blame the caller for our outage.
                match (&http_err, &js_err) {
                    (CrwError::TargetUnreachable(_), js) if is_origin_navigation_failure(js) => {
                        http_err
                    }
                    _ => js_err,
                }
            })
    }

    async fn fetch_with_js(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
        wait_for_ms: Option<u64>,
        requested_renderer: Option<&str>,
        deadline: crw_core::Deadline,
    ) -> CrwResult<FetchResult> {
        let host = host_of(url);
        let is_user_pinned = matches!(requested_renderer, Some(name) if name != "auto");
        if let Some(pinned) = requested_renderer
            && let Some(kind) = renderer_kind_for(pinned)
        {
            metrics()
                .user_pin_total
                .with_label_values(&[kind.as_str()])
                .inc();
        }

        // Filter the JS pool down to a hard-pinned renderer when one was named.
        // "auto" or `None` means "use the configured chain".
        let mut renderers: Vec<&Arc<dyn PageFetcher>> = match requested_renderer {
            Some(name) if name != "auto" => self
                .js_renderers
                .iter()
                .filter(|r| r.name() == name)
                .collect(),
            _ => self.js_renderers.iter().collect(),
        };

        // Auto mode: if this host has been promoted, try Chrome first.
        if !is_user_pinned
            && let Some(RendererKind::Chrome) = self.preferences.preferred(&host).await
        {
            // 3-tier rank: chrome first, then the residential chrome_proxy,
            // then everything lighter. A stable binary key would yield
            // `[chrome, lightpanda, chrome_proxy]` — escalating a chrome
            // block to lightpanda (same WAF, lighter fingerprint) before
            // ever reaching the residential tier.
            renderers.sort_by_key(|r| match r.name() {
                "chrome" => 0,
                "chrome_proxy" => 1,
                _ => 2,
            });
            tracing::debug!(host = %host, "host promoted to chrome by preference learner");
        }

        if renderers.is_empty() {
            let available = self.js_renderer_names();
            return Err(CrwError::RendererError(format!(
                "requested renderer '{}' not in pool [{}]",
                requested_renderer.unwrap_or("auto"),
                available.join(", ")
            )));
        }

        // Track the chain we attempted so we can populate
        // `RenderDecision::Failover` when nothing succeeded outright.
        let mut chain: Vec<RendererKind> = Vec::new();
        let mut breaker_skipped: Vec<RendererKind> = Vec::new();
        let mut last_error = None;
        let mut last_failover_reason: Option<FailoverErrorKind> = None;
        let mut thin_result: Option<FetchResult> = None;
        // Snapshot for the leak-through fallback below. The main loop
        // consumes `renderers`; we keep a parallel reference list so a
        // single skipped renderer can still get a shot when its host
        // breaker is closed.
        let renderers_snapshot: Vec<&Arc<dyn PageFetcher>> = renderers.clone();

        for renderer in renderers {
            let kind = renderer_kind_for(renderer.name());

            // Skip empty hosts: don't pollute breaker/preference caches
            // with the "" key when URL parsing failed.
            let trackable = kind.filter(|_| !host.is_empty());

            // A tier-side skip on a *partial* budget stays removed: letting a
            // renderer attempt with a partial-DOM budget beats aborting pre-flight
            // on legitimately-slow tail URLs, and classify_outcome ignores
            // DeadlineClamped so the breaker isn't poisoned. What is reinstated
            // here is narrower: a *degenerate* budget. A CDP attempt cannot even
            // finish its handshake in single-digit milliseconds, so it returns a
            // fabricated `Timeout after 5ms` that pollutes logs and burns a pool
            // slot. Measured upstream in prod: 432 of 536 escalations ran with
            // <50ms of budget. Skip those, and only those.
            //
            // Note this is skip-*without*-attempting, distinct from the post-hoc
            // DeadlineClamped classification, which still only applies to tiers
            // that were actually invoked.
            let remaining = deadline.remaining();
            if remaining < MIN_TIER_BUDGET {
                tracing::info!(
                    renderer = renderer.name(),
                    remaining_ms = remaining.as_millis() as u64,
                    "budget below minimum tier budget, skipping renderer"
                );
                if let Some(k) = kind {
                    // Deliberately NOT `breaker_skipped`: that vec means "the circuit
                    // breaker rejected this tier" and gates the leak-through arm.
                    metrics()
                        .render_route_decision_total
                        .with_label_values(&[k.as_str(), "budgetSkipped"])
                        .inc();
                }
                // Preserve the status code a starved request returns today. The tier we
                // are skipping would have been invoked with `remaining`, timed out, and
                // written `CrwError::Timeout` here — overwriting any earlier error, as
                // every other `last_error` assignment in this function does. Assign
                // unconditionally for the same reason: `get_or_insert_with` would let an
                // earlier tier's `RendererError` survive and map to 500 instead of 504.
                //
                // Report the REQUESTED budget, not `remaining`: that is below
                // MIN_TIER_BUDGET by definition here and ~0 in practice, so it read as
                // `Timeout after 1ms` to a caller given 30s.
                last_error = Some(CrwError::Timeout(deadline.requested_ms()));
                continue;
            }

            // Consult breaker for tracked renderers. Untracked names (e.g.
            // "playwright") bypass the breaker for now.
            let mut probe_guard: Option<ProbeGuard> = None;
            if let Some(k) = trackable {
                let (permit, guard) = self.breakers.acquire_with_guard(&host, k).await;
                if permit == Permit::Rejected {
                    tracing::info!(
                        renderer = renderer.name(),
                        host = %host,
                        "circuit breaker open, skipping renderer"
                    );
                    metrics()
                        .render_route_decision_total
                        .with_label_values(&[k.as_str(), "breakerSkipped"])
                        .inc();
                    breaker_skipped.push(k);
                    drop(guard); // not Probe — drop is a no-op
                    continue;
                }
                probe_guard = Some(guard);
            }

            // `acquire_with_guard` awaits, so the budget may have drained while we
            // waited for a breaker permit. Re-check before dispatching, or the floor
            // above is only advisory. Dropping `probe_guard` here cancels the probe
            // (see `ProbeGuard::drop`), so the breaker is left as we found it.
            let remaining = deadline.remaining();
            if remaining < MIN_TIER_BUDGET {
                tracing::info!(
                    renderer = renderer.name(),
                    remaining_ms = remaining.as_millis() as u64,
                    "budget drained while acquiring breaker permit, skipping renderer"
                );
                if let Some(k) = kind {
                    metrics()
                        .render_route_decision_total
                        .with_label_values(&[k.as_str(), "budgetSkipped"])
                        .inc();
                }
                // Same reasoning as the floor above: report the requested budget,
                // never the sub-minimum remainder.
                last_error = Some(CrwError::Timeout(deadline.requested_ms()));
                continue;
            }

            if let Some(k) = kind {
                chain.push(k);
            }

            // Capture pre-call context so post-await classification is
            // race-free against deadline drift.
            let attempt_ctx = {
                let remaining = deadline.remaining();
                let tier_budget = kind
                    .and_then(|k| self.tier_timeouts.get(&k).copied())
                    .unwrap_or(remaining);
                AttemptContext::capture(remaining, tier_budget)
            };
            match renderer.fetch(url, headers, wait_for_ms, deadline).await {
                Ok(mut result) => {
                    let text_len = html_body_text_len(&result.html);
                    let is_placeholder = detector::looks_like_loading_placeholder(&result.html);
                    let failed_render = detector::looks_like_failed_render(&result.html);
                    let is_bot_wall = detector::looks_like_generic_bot_wall(&result.html);
                    let vendor_block = detector::looks_like_vendor_block(&result.html);
                    // Size-independent Cloudflare interstitial check: modern
                    // managed challenges are 100-300KB with the challenge marker
                    // deep in the body, which the size-capped detectors above
                    // miss — the challenge text would be returned as content.
                    let cf_challenge = detector::looks_like_cloudflare_challenge(&result.html);
                    // Mirrors the HTTP-tier escalation set (lib.rs:658). A JS
                    // renderer can return 200 with bot HTML or 403 with content
                    // — without this check, both slip through as "valid".
                    let is_status_blocked = matches!(
                        result.status_code,
                        401 | 403 | 404 | 405 | 406 | 410 | 412 | 429 | 451 | 500 | 503
                    );
                    // The comprehensive 3-tier antibot classifier. The
                    // `detector` heuristics above only know a fixed phrase
                    // list + 8 named vendors; `classify()` additionally
                    // recognises Reddit-class WAF pages ("blocked by network
                    // security") served with HTTP 200 that otherwise slip
                    // through as success. Always runs for telemetry when
                    // `enabled`; only forces escalation when
                    // `escalate_in_failover` is on (the kill switch).
                    let antibot = if self.antibot.enabled {
                        crw_extract::antibot::classify(Some(result.status_code), &result.html)
                    } else {
                        crw_extract::antibot::AntibotResult::none()
                    };
                    let antibot_blocked =
                        self.antibot.escalate_in_failover && antibot.signal.is_blocked();
                    if text_len >= Self::MIN_RENDERED_TEXT_LEN
                        && !is_placeholder
                        && failed_render.is_none()
                        && !is_bot_wall
                        && vendor_block.is_none()
                        && !cf_challenge
                        && !is_status_blocked
                        && !antibot_blocked
                    {
                        // Capture the promotion state BEFORE record_success
                        // clears the latch — otherwise AutoPromoted decisions
                        // race against the success path and downgrade to AutoDefault.
                        let was_promoted = matches!(
                            self.preferences.preferred(&host).await,
                            Some(RendererKind::Chrome)
                        );
                        if let Some(k) = trackable {
                            // Treat truncated-but-valid as Truncated (ignored
                            // by default per BreakerConfig.count_truncated_as_failure).
                            let outcome = if result.truncated {
                                BreakerOutcome::Truncated
                            } else {
                                BreakerOutcome::Success
                            };
                            self.breakers.record_outcome(&host, k, outcome).await;
                            self.preferences.record_success(&host).await;
                            metrics()
                                .render_route_decision_total
                                .with_label_values(&[k.as_str(), "success"])
                                .inc();
                            metrics()
                                .host_preferences_size
                                .set(self.preferences.size() as i64);
                        }
                        if let Some(g) = probe_guard.take() {
                            g.disarm();
                        }
                        // Populate routing metadata + per-renderer credit.
                        if let Some(k) = kind {
                            result.credit_cost = credit_for(k);
                            result.render_decision = Some(if is_user_pinned {
                                RenderDecision::UserPinned { renderer: k }
                            } else if !breaker_skipped.is_empty() {
                                RenderDecision::BreakerSkipped {
                                    skipped: breaker_skipped[0],
                                    chosen: k,
                                }
                            } else if chain.len() > 1 {
                                RenderDecision::Failover {
                                    chain: chain.clone(),
                                    reason: last_failover_reason
                                        .clone()
                                        .unwrap_or(FailoverErrorKind::Other),
                                }
                            } else if was_promoted && k == RendererKind::Chrome {
                                RenderDecision::AutoPromoted {
                                    chosen: k,
                                    from: RendererKind::Lightpanda,
                                    reason: "host preference learner".into(),
                                }
                            } else {
                                RenderDecision::AutoDefault { chosen: k }
                            });
                        }
                        return Ok(result);
                    }
                    // Treat thin/placeholder/failed as a soft failure for
                    // breaker + preference purposes.
                    let err_kind = match failed_render {
                        Some(detector::FailedRenderReason::NextJsClientError) => {
                            FailoverErrorKind::NextJsClientError
                        }
                        Some(detector::FailedRenderReason::ReactMinifiedError) => {
                            FailoverErrorKind::NextJsClientError
                        }
                        Some(detector::FailedRenderReason::EmptyNextRoot) => {
                            FailoverErrorKind::EmptyNextRoot
                        }
                        None if vendor_block.is_some() => FailoverErrorKind::VendorBlock,
                        None if is_status_blocked => FailoverErrorKind::StatusBlocked,
                        None if is_placeholder => FailoverErrorKind::PlaceholderContent,
                        None if is_bot_wall => FailoverErrorKind::PlaceholderContent,
                        // The classifier caught a block the detector missed.
                        None if antibot_blocked => FailoverErrorKind::AntibotBlock,
                        None => FailoverErrorKind::PlaceholderContent,
                    };
                    last_failover_reason = Some(err_kind.clone());
                    if let Some(k) = trackable {
                        // Thin/placeholder/failed render → classify against
                        // attempt context so deadline-clamped attempts don't
                        // poison the breaker.
                        let outcome = classify_outcome(false, false, false, &attempt_ctx);
                        // Host-scoped: the tier answered, and this is a verdict on
                        // the body it returned. Written to the global window, one
                        // busy domain reaches `min_calls` on its own and disables
                        // the tier for every host.
                        self.breakers
                            .record_scoped_outcome(&host, k, None, Some(outcome))
                            .await;
                        if k == RendererKind::Lightpanda
                            && let Some(target) =
                                self.preferences.record_failure(&host, &err_kind).await
                        {
                            metrics()
                                .host_preferences_promotions_total
                                .with_label_values(&[k.as_str(), target.as_str()])
                                .inc();
                            tracing::info!(
                                host = %host,
                                "host promoted by preference learner: {} -> {}",
                                k.as_str(),
                                target.as_str()
                            );
                        }
                    }
                    if let Some(g) = probe_guard.take() {
                        g.disarm();
                    }
                    if let Some(vendor) = vendor_block {
                        metrics()
                            .vendor_block_total
                            .with_label_values(&[vendor])
                            .inc();
                        tracing::warn!(
                            renderer = renderer.name(),
                            url,
                            vendor,
                            "vendor anti-bot block detected"
                        );
                    }
                    // Emit the antibot signal regardless of `escalate_in_failover`
                    // — a pre-flip dashboard of escalation pressure.
                    if antibot.signal.is_blocked() {
                        metrics()
                            .antibot_escalation_total
                            .with_label_values(&[antibot.signal.class_name()])
                            .inc();
                        tracing::warn!(
                            renderer = renderer.name(),
                            url,
                            signal = antibot.signal.class_name(),
                            reason = %antibot.reason,
                            status_code = result.status_code,
                            text_len,
                            escalated = antibot_blocked,
                            "antibot classifier flagged a block"
                        );
                    }
                    tracing::info!(
                        renderer = renderer.name(),
                        text_len,
                        is_placeholder,
                        is_bot_wall,
                        vendor_block,
                        is_status_blocked,
                        antibot_signal = antibot.signal.class_name(),
                        antibot_blocked,
                        status_code = result.status_code,
                        failed_render = ?failed_render,
                        "JS renderer returned thin/placeholder/failed content, trying next renderer"
                    );
                    // Annotate the result so it can surface through `thin_result`
                    // if no later renderer succeeds. Preserves any warning the
                    // renderer set, but adds the failover reason. We keep the
                    // first thin result as the body to return (no point in
                    // accumulating bodies), but stitch later renderers'
                    // warnings onto it so debug output reflects every attempt.
                    let mut annotated = result;
                    let attempt_warning = if let Some(reason) = failed_render {
                        format!(
                            "{} returned a failed render ({})",
                            renderer.name(),
                            reason.as_str()
                        )
                    } else if is_placeholder {
                        format!("{} returned a loading placeholder", renderer.name())
                    } else if let Some(vendor) = vendor_block {
                        format!(
                            "{} returned a vendor anti-bot block ({vendor})",
                            renderer.name()
                        )
                    } else if is_bot_wall {
                        format!(
                            "{} returned a generic anti-bot interstitial",
                            renderer.name()
                        )
                    } else if is_status_blocked {
                        format!(
                            "{} returned HTTP {} (treated as blocked)",
                            renderer.name(),
                            annotated.status_code
                        )
                    } else if antibot_blocked {
                        format!(
                            "{} returned an anti-bot block ({}: {})",
                            renderer.name(),
                            antibot.signal.class_name(),
                            antibot.reason
                        )
                    } else {
                        format!(
                            "{} returned thin content (text_len={text_len})",
                            renderer.name()
                        )
                    };
                    if is_bot_wall || vendor_block.is_some() || is_status_blocked || antibot_blocked
                    {
                        // Surface bot-wall as a RendererError so, if every
                        // renderer in the chain hits a wall, the final error
                        // (line ~1052) carries an actionable message.
                        // RendererError maps to FailoverErrorKind::LightpandaCrash
                        // via classify_renderer_error — that's intentional:
                        // bot-wall hosts SHOULD be promoted to Chrome by the
                        // host preference learner, since LightPanda lacks the
                        // TLS/header fingerprint to clear them.
                        let msg = if let Some(v) = vendor_block {
                            format!("{} returned a vendor anti-bot block ({v})", renderer.name())
                        } else if is_status_blocked {
                            format!(
                                "{} returned HTTP {} (treated as blocked)",
                                renderer.name(),
                                annotated.status_code
                            )
                        } else if is_bot_wall {
                            format!(
                                "{} returned a generic anti-bot interstitial",
                                renderer.name()
                            )
                        } else {
                            format!(
                                "{} returned an anti-bot block ({}: {})",
                                renderer.name(),
                                antibot.signal.class_name(),
                                antibot.reason
                            )
                        };
                        last_error = Some(CrwError::RendererError(msg));
                    }
                    annotated.warnings.push(attempt_warning.clone());
                    annotated.warning = Some(match annotated.warning {
                        Some(prev) => format!("{prev}; {attempt_warning}"),
                        None => attempt_warning.clone(),
                    });
                    thin_result = Some(match thin_result {
                        None => annotated,
                        Some(existing) => {
                            // Prefer the larger HTML when stitching thin
                            // results — a later renderer (e.g. chrome) often
                            // returns a CAPTCHA shell that, while small,
                            // contains anti-bot markers absent from an even
                            // smaller earlier shell. Diagnostics & block
                            // detection then have something to match on.
                            let (mut keeper, dropped) =
                                if annotated.html.len() > existing.html.len() {
                                    (annotated, existing)
                                } else {
                                    (existing, annotated)
                                };
                            keeper.warnings.push(attempt_warning.clone());
                            keeper.warning = Some(match keeper.warning {
                                Some(prev) => format!("{prev}; {attempt_warning}"),
                                None => attempt_warning,
                            });
                            // Carry over any extra warnings from the dropped
                            // attempt so debug output stays complete.
                            for w in dropped.warnings {
                                if !keeper.warnings.contains(&w) {
                                    keeper.warnings.push(w);
                                }
                            }
                            keeper
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(renderer = renderer.name(), "JS renderer failed: {e}");
                    let err_kind = classify_renderer_error(&e);
                    last_failover_reason = Some(err_kind.clone());
                    if let Some(k) = trackable {
                        let was_timeout = matches!(e, CrwError::Timeout(_));
                        let outcome = classify_outcome(false, false, was_timeout, &attempt_ctx);
                        // A renderer error with no response to inspect is not proof
                        // the TIER is sick: a dead origin produces the same shape,
                        // and every tier egressing from this box sees it. Origin
                        // faults stay host-scoped. Timeouts are excluded on purpose:
                        // a hung CDP pool also times out, and that IS a tier signal.
                        let global =
                            (!is_origin_fault_for_breaker(&e) || was_timeout).then_some(outcome);
                        self.breakers
                            .record_scoped_outcome(&host, k, global, Some(outcome))
                            .await;
                        if k == RendererKind::Lightpanda {
                            let _ = self.preferences.record_failure(&host, &err_kind).await;
                        }
                    }
                    if let Some(g) = probe_guard.take() {
                        g.disarm();
                    }
                    last_error = Some(e);
                    continue;
                }
            }
        }
        // Leak-through fallback: every renderer was rejected by the global
        // breaker, but the host itself has no failures recorded. Rather
        // than fail the request outright (which is what made the bench
        // shed ~12% on broad lightpanda outages), give one renderer a
        // single attempt without recording its outcome to the global
        // window. The host tier still records, so a host that's actually
        // broken trips its own breaker on the next attempt.
        // Trigger when every chain attempt failed outright (no thin_result,
        // no Ok return) AND at least one renderer was skipped by the global
        // breaker. Common case: lightpanda runs and errors, chrome gets
        // globally rejected → without leak we'd return error even though
        // chrome's host breaker is clean and would likely succeed.
        //
        // Skip when the request deadline is already (near-)exhausted:
        // entering a renderer with <500ms budget produced 37/128 of the
        // first leak run's failures as "Timeout after 1-2ms" — the
        // attempt cannot succeed and just consumes a CDP connection.
        // (Same reasoning now guards the main ladder loop; see MIN_TIER_BUDGET.)
        if thin_result.is_none()
            && !breaker_skipped.is_empty()
            && !is_user_pinned
            && deadline.remaining() >= MIN_TIER_BUDGET
        {
            for renderer in &renderers_snapshot {
                let kind = renderer_kind_for(renderer.name());
                let trackable = kind.filter(|_| !host.is_empty());
                let Some(k) = trackable else { continue };
                if !breaker_skipped.contains(&k) {
                    continue;
                }
                let permit = self.breakers.try_acquire_host_only(&host, k).await;
                if permit == Permit::Rejected {
                    continue;
                }
                // That acquire awaits; re-check the budget before dispatching so the
                // floor above is not merely advisory (same TOCTOU as the serial loop).
                if deadline.remaining() < MIN_TIER_BUDGET {
                    continue;
                }
                tracing::info!(
                    renderer = renderer.name(),
                    host = %host,
                    "global breaker open, host clean — leaking through one attempt"
                );
                metrics()
                    .render_route_decision_total
                    .with_label_values(&[k.as_str(), "leakThrough"])
                    .inc();
                let attempt_ctx = {
                    let remaining = deadline.remaining();
                    let tier_budget = self.tier_timeouts.get(&k).copied().unwrap_or(remaining);
                    AttemptContext::capture(remaining, tier_budget)
                };
                let res = renderer.fetch(url, headers, wait_for_ms, deadline).await;
                match res {
                    Ok(mut result) => {
                        let text_len = html_body_text_len(&result.html);
                        let is_placeholder = detector::looks_like_loading_placeholder(&result.html);
                        let failed_render = detector::looks_like_failed_render(&result.html);
                        let truncated = result.truncated;
                        // A large CF challenge shell has body text > 50 and no
                        // placeholder/failed marker, so guard it explicitly or it
                        // would leak through this path as success.
                        let content_ok = text_len >= Self::MIN_RENDERED_TEXT_LEN
                            && !is_placeholder
                            && failed_render.is_none()
                            && !detector::looks_like_cloudflare_challenge(&result.html);
                        let outcome = classify_outcome(content_ok, truncated, false, &attempt_ctx);
                        // Record host only — global stays untouched so the
                        // existing trip can finish its cooldown naturally.
                        self.breakers
                            .record_scoped_outcome(&host, k, None, Some(outcome))
                            .await;
                        if content_ok {
                            result.credit_cost = credit_for(k);
                            result.render_decision =
                                Some(RenderDecision::AutoDefault { chosen: k });
                            return Ok(result);
                        }
                        // Thin/placeholder on leak path → fall through to
                        // the normal "no JS renderer" return below.
                        last_error = Some(CrwError::RendererError(format!(
                            "leak attempt on {} returned thin content (text_len={text_len})",
                            renderer.name()
                        )));
                        break;
                    }
                    Err(e) => {
                        let was_timeout = matches!(e, CrwError::Timeout(_));
                        let outcome = classify_outcome(false, false, was_timeout, &attempt_ctx);
                        self.breakers
                            .record_scoped_outcome(&host, k, None, Some(outcome))
                            .await;
                        last_error = Some(e);
                        break;
                    }
                }
            }
        }

        // Return the best thin result if we have one, otherwise the last error.
        if let Some(mut result) = thin_result {
            // Stamp routing metadata on the soft-failure result too — callers
            // need to know which chain was attempted for debugging.
            if let Some(last) = chain.last().copied() {
                result.credit_cost = credit_for(last);
                result.render_decision = Some(RenderDecision::Failover {
                    chain: chain.clone(),
                    reason: last_failover_reason
                        .clone()
                        .unwrap_or(FailoverErrorKind::Other),
                });
            }
            // When the user hard-pinned a single renderer and it failed thin,
            // failover never ran — surface an actionable hint so callers (SaaS
            // playground, CLI, MCP) can show a banner instead of silently
            // returning broken markdown with `success: true`.
            if is_user_pinned
                && chain.len() == 1
                && let Some(pinned) = chain.first().copied()
            {
                let reason = last_failover_reason
                    .as_ref()
                    .map(|r| r.as_str())
                    .unwrap_or("unknown");
                let hint = format!(
                    "Pinned renderer '{}' returned a failed render ({}). Content may be unreliable. Retry with renderer=\"chrome\" or omit the renderer field for auto-failover.",
                    pinned.as_str(),
                    reason,
                );
                result.warnings.push(hint);
            }
            // A wall no tier could clear is not content. Every tier already
            // classified it — that verdict is exactly why the result landed in
            // `thin_result` instead of being accepted — and this was the one place
            // that discarded it: the interstitial went back under `success: true`
            // and an agent read it as the page.
            //
            // Scoped deliberately to the generic phrase-list wall. A genuinely thin
            // but real page still ships, because returning the best available body
            // is the point of this tail.
            if detector::looks_like_generic_bot_wall(&result.html) {
                return Err(CrwError::HttpError(format!(
                    "blocked by an anti-bot wall that none of the {} renderer tier(s) \
                     attempted could clear",
                    chain.len().max(1),
                )));
            }
            Ok(result)
        } else {
            Err(last_error
                .unwrap_or_else(|| CrwError::RendererError("No JS renderer available".to_string())))
        }
    }

    /// Check availability of all renderers.
    pub async fn check_health(&self) -> HashMap<String, bool> {
        let mut health = HashMap::new();
        health.insert("http".to_string(), self.http.is_available().await);
        for r in &self.js_renderers {
            health.insert(r.name().to_string(), r.is_available().await);
        }
        health
    }
}

/// Rough estimate of visible text length in an HTML document.
/// Strips tags and collapses whitespace. Used to detect "thin" renders
/// where a renderer returned HTML but failed to execute JavaScript.
fn html_body_text_len(html: &str) -> usize {
    // Extract body content if present, otherwise use entire HTML.
    let body = if let Some(start) = html.find("<body") {
        let start = html[start..].find('>').map(|i| start + i + 1).unwrap_or(0);
        let end = html.find("</body>").unwrap_or(html.len());
        &html[start..end]
    } else {
        html
    };
    // Strip tags crudely.
    let mut in_tag = false;
    let mut text_len = 0;
    let mut prev_ws = true;
    for ch in body.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            if ch.is_whitespace() {
                if !prev_ws {
                    text_len += 1;
                    prev_ws = true;
                }
            } else {
                text_len += 1;
                prev_ws = false;
            }
        }
    }
    text_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::BreakerConfig;
    #[cfg(feature = "cdp")]
    use crw_core::config::CdpEndpoint;
    use std::time::Duration;

    /// Generous deadline used by tests that don't care about budget enforcement.
    fn tdl() -> crw_core::Deadline {
        crw_core::Deadline::now_plus(Duration::from_secs(60))
    }

    fn base_cfg(mode: RendererMode) -> RendererConfig {
        RendererConfig {
            mode,
            ..Default::default()
        }
    }

    #[test]
    fn new_mode_none_ok_no_js_renderers() {
        let cfg = base_cfg(RendererMode::None);
        let r = FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default()).unwrap();
        assert!(r.js_renderer_names().is_empty());
        assert_eq!(r.render_js_default, None);
    }

    /// The escalation after a thin LightPanda body must name a tier the pool
    /// holds. Pinning a name it does not hold is a hard error, so the old
    /// literal "chrome" failed every escalation on this fork's ladder.
    #[cfg(all(feature = "cdp", feature = "camofox"))]
    #[test]
    fn lightpanda_escalation_target_picks_a_tier_the_pool_actually_holds() {
        let target = |cfg: &RendererConfig| {
            let r =
                FallbackRenderer::new(cfg, "crw-test", None, &StealthConfig::default()).unwrap();
            r.lightpanda_escalation_target().map(str::to_string)
        };
        let lp = || {
            Some(CdpEndpoint {
                ws_url: "ws://127.0.0.1:9222/".into(),
            })
        };

        // Fork ladder: lightpanda then camofox. The escalation lands on camofox.
        assert_eq!(
            target(&RendererConfig {
                mode: RendererMode::Auto,
                lightpanda: lp(),
                camofox: Some(crw_core::config::CamofoxEndpoint {
                    base_url: "http://127.0.0.1:9377".into(),
                    api_key: None,
                }),
                ..Default::default()
            }),
            Some("camofox".to_string())
        );

        // Nothing above lightpanda: no target, so no unsatisfiable pin.
        assert_eq!(
            target(&RendererConfig {
                mode: RendererMode::Auto,
                lightpanda: lp(),
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn new_mode_auto_no_endpoints_ok_http_only() {
        let cfg = base_cfg(RendererMode::Auto);
        let r = FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default()).unwrap();
        assert!(r.js_renderer_names().is_empty());
    }

    #[cfg(feature = "cdp")]
    #[test]
    fn new_mode_lightpanda_without_endpoint_errors() {
        let cfg = base_cfg(RendererMode::Lightpanda);
        let err =
            FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default()).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("lightpanda"));
    }

    #[cfg(feature = "cdp")]
    #[test]
    fn new_mode_auto_with_lightpanda_endpoint_builds_lightpanda() {
        let cfg = RendererConfig {
            mode: RendererMode::Auto,
            lightpanda: Some(CdpEndpoint {
                ws_url: "ws://127.0.0.1:9222/".into(),
            }),
            ..Default::default()
        };
        let r = FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default()).unwrap();
        assert_eq!(r.js_renderer_names(), vec!["lightpanda"]);
    }

    #[cfg(not(feature = "cdp"))]
    #[test]
    fn new_mode_chrome_errors_without_cdp_feature() {
        let cfg = base_cfg(RendererMode::Chrome);
        let err =
            FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default()).unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("cdp"), "expected cdp in error: {msg}");
    }

    #[test]
    fn new_render_js_default_stored() {
        let cfg = RendererConfig {
            mode: RendererMode::None,
            render_js_default: Some(true),
            ..Default::default()
        };
        let r = FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default()).unwrap();
        assert_eq!(r.render_js_default, Some(true));
    }

    /// Mock fetcher for unit-testing dispatch logic without real CDP/HTTP.
    struct MockFetcher {
        name: &'static str,
        behavior: MockBehavior,
    }

    #[derive(Clone)]
    enum MockBehavior {
        Ok(String),
        OkStatus(u16, String),
        Err(String),
        Timeout,
    }

    #[async_trait::async_trait]
    impl PageFetcher for MockFetcher {
        async fn fetch(
            &self,
            url: &str,
            _headers: &HashMap<String, String>,
            _wait_for_ms: Option<u64>,
            _deadline: crw_core::Deadline,
        ) -> CrwResult<FetchResult> {
            let (status, html) = match &self.behavior {
                MockBehavior::Ok(html) => (200u16, html.clone()),
                MockBehavior::OkStatus(s, html) => (*s, html.clone()),
                MockBehavior::Err(msg) => return Err(CrwError::RendererError(msg.clone())),
                MockBehavior::Timeout => return Err(CrwError::Timeout(2_500)),
            };
            Ok(FetchResult {
                url: url.to_string(),
                final_url: None,
                status_code: status,
                html,
                content_type: Some("text/html".to_string()),
                raw_bytes: None,
                rendered_with: Some(self.name.to_string()),
                elapsed_ms: 0,
                warning: None,
                render_decision: None,
                credit_cost: 0,
                warnings: Vec::new(),
                truncated: false,
                deadline_exceeded: false,
                captured_responses: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            self.name
        }
        fn supports_js(&self) -> bool {
            true
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    fn rich_html(marker: &str) -> String {
        format!(
            "<html><body><article>{}{}</article></body></html>",
            marker,
            "x".repeat(200)
        )
    }

    fn make_renderer_with_mocks(mocks: Vec<Arc<dyn PageFetcher>>) -> FallbackRenderer {
        // Build a real HTTP fetcher (won't be hit when render_js=Some(true)).
        let cfg = base_cfg(RendererMode::None);
        let mut r =
            FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default()).unwrap();
        r.js_renderers = mocks;
        r
    }

    /// Mock that records whether it was invoked. Separate from `MockFetcher` so the
    /// existing constructor sites stay untouched.
    struct CountingFetcher {
        name: &'static str,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PageFetcher for CountingFetcher {
        async fn fetch(
            &self,
            url: &str,
            _headers: &HashMap<String, String>,
            _wait_for_ms: Option<u64>,
            _deadline: crw_core::Deadline,
        ) -> CrwResult<FetchResult> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(FetchResult {
                url: url.to_string(),
                final_url: None,
                status_code: 200,
                html: rich_html("rendered"),
                content_type: Some("text/html".to_string()),
                raw_bytes: None,
                rendered_with: Some(self.name.to_string()),
                elapsed_ms: 0,
                warning: None,
                render_decision: None,
                credit_cost: 0,
                warnings: Vec::new(),
                truncated: false,
                deadline_exceeded: false,
                captured_responses: Vec::new(),
            })
        }
        fn name(&self) -> &str {
            self.name
        }
        fn supports_js(&self) -> bool {
            true
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    /// A degenerate budget must not invoke a JS tier at all, and the request must
    /// still surface `CrwError::Timeout` so the server keeps mapping it to 504
    /// rather than a 500 from the `RendererError` tail.
    #[tokio::test]
    async fn degenerate_budget_skips_js_tier_and_preserves_timeout() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock = Arc::new(CountingFetcher {
            name: "chrome",
            calls: calls.clone(),
        });
        let r = make_renderer_with_mocks(vec![mock]);

        let err = r
            .fetch_with_js(
                "https://example.com",
                &HashMap::new(),
                None,
                None,
                crw_core::Deadline::from_request_ms(0),
            )
            .await
            .expect_err("an exhausted budget must not produce a rendered page");

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "renderer must be skipped, not invoked with a few milliseconds"
        );
        assert!(
            matches!(err, CrwError::Timeout(_)),
            "must stay a Timeout (504), not RendererError (500); got {err:?}"
        );
    }

    /// Burns most of the budget, then fails — so the NEXT tier lands below the floor.
    struct SlowFailingFetcher {
        name: &'static str,
        burn: Duration,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PageFetcher for SlowFailingFetcher {
        async fn fetch(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
            _wait_for_ms: Option<u64>,
            _deadline: crw_core::Deadline,
        ) -> CrwResult<FetchResult> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(self.burn).await;
            Err(CrwError::RendererError("anti-bot wall".to_string()))
        }
        fn name(&self) -> &str {
            self.name
        }
        fn supports_js(&self) -> bool {
            true
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    /// A tier that keeps returning THIN content must not be disabled for every
    /// host, and this pins it at the call site rather than at the registry.
    ///
    /// The registry-level tests only prove `record_scoped_outcome` behaves; they
    /// pass whether or not the ladder actually calls it. A mutation reverting the
    /// serial thin arm to `record_outcome` left all 995 other tests green, which
    /// is the gap this closes. A thin body is a verdict about the PAGE, so it
    /// belongs to the host tier: the 1000-URL bench traced roughly 12% of
    /// failures to false global trips from exactly this.
    #[tokio::test]
    async fn thin_content_does_not_open_the_global_tier() {
        // Under MIN_RENDERED_TEXT_LEN (50), so every attempt classifies as thin
        // rather than acceptable, and nothing here is a wall or a hard block.
        let thin = "<html><body>tiny</body></html>";
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(thin.to_string()),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "camofox",
            behavior: MockBehavior::Ok(thin.to_string()),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![lp, chrome]);
        r.breakers = Arc::new(BreakerRegistry::new(BreakerConfig {
            base_cooldown: Duration::from_secs(300),
            max_cooldown: Duration::from_secs(300),
            ..BreakerConfig::default()
        }));

        for _ in 0..80 {
            let _ = r
                .fetch(
                    "https://thin-host.example/page",
                    &HashMap::new(),
                    Some(true),
                    None,
                    None,
                    tdl(),
                )
                .await;
        }

        assert_eq!(
            r.breakers
                .global_for(RendererKind::Lightpanda)
                .snapshot()
                .state,
            "closed",
            "a thin body is a judgement about the page, so it must not disable \
             the tier for every other host"
        );
        assert_eq!(
            r.breakers
                .host_for("thin-host.example", RendererKind::Lightpanda)
                .await
                .snapshot()
                .state,
            "open",
            "the host tier must still learn this host renders thin"
        );
    }

    /// The other half of the origin carve-out, and the one a mutation test showed
    /// nothing was guarding.
    ///
    /// `is_origin_fault_for_breaker` returns true for `CrwError::Timeout(_)`,
    /// because it is shared with the error-attribution path where that arm is
    /// correct. The breaker path must undo it with `&& !was_timeout`, or a hung
    /// CDP pool, which times out on every host, would stop reaching the global
    /// window and nothing would ever conclude the tier is sick. Dropping that
    /// clause is a one-token mutation that left all 995 other tests green, which
    /// is exactly why this test exists.
    #[tokio::test]
    async fn tier_timeouts_still_open_the_global_tier() {
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Timeout,
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "camofox",
            behavior: MockBehavior::Timeout,
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![lp, chrome]);
        r.breakers = Arc::new(BreakerRegistry::new(BreakerConfig {
            base_cooldown: Duration::from_secs(300),
            max_cooldown: Duration::from_secs(300),
            ..BreakerConfig::default()
        }));

        // Spread across distinct hosts, which is what a genuinely sick tier looks
        // like and what a single dead origin does not.
        for i in 0..80 {
            let _ = r
                .fetch(
                    &format!("https://host{i}.example/page"),
                    &HashMap::new(),
                    Some(true),
                    None,
                    None,
                    tdl(),
                )
                .await;
        }

        assert_eq!(
            r.breakers
                .global_for(RendererKind::Lightpanda)
                .snapshot()
                .state,
            "open",
            "a tier timing out on every host must still trip globally, or a hung \
             pool is never caught"
        );
    }

    /// A dead ORIGIN must not disable a renderer tier for every other host.
    ///
    /// The serial error arm used to write its outcome to the global window as
    /// well as the host one, on the stated premise that "the renderer itself
    /// errored (no response to inspect), which is a genuine tier signal". A dead
    /// origin produces exactly that shape: six hours of production held 24
    /// net::ERR_ABORTED, 10 PeerFailedVerification, 5 ERR_CERT_DATE_INVALID and
    /// more, every one the target's fault and every one advancing the global
    /// window. Two of the four observed global trips came from here.
    ///
    /// Eighty such failures across eighty DISTINCT hosts is far past
    /// `min_calls: 50` at `failure_rate_threshold: 0.80`, so this test fails
    /// against the old code and passes only once the outcome is host-scoped.
    #[tokio::test]
    async fn origin_navigation_failures_do_not_open_the_global_tier() {
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Err(
                "Renderer error: Navigation failed: net::ERR_CONNECTION_REFUSED".into(),
            ),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "camofox",
            behavior: MockBehavior::Ok(rich_html("CAMOFOX-OK")),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![lp, chrome]);
        // Long cooldown so a half-open transition cannot mask the verdict under
        // parallel test load, matching the sibling breaker test above.
        r.breakers = Arc::new(BreakerRegistry::new(BreakerConfig {
            base_cooldown: Duration::from_secs(300),
            max_cooldown: Duration::from_secs(300),
            ..BreakerConfig::default()
        }));

        // One dead origin, hammered. 80 failures is past `min_calls: 50` at
        // `failure_rate_threshold: 0.80`, so this is exactly the shape that used
        // to take the tier out for everyone: in production one domain
        // contributed 38 of the ~214 lightpanda outcomes that reached any window
        // in six hours.
        for _ in 0..80 {
            let _ = r
                .fetch(
                    "https://dead-origin.example/page",
                    &HashMap::new(),
                    Some(true),
                    None,
                    None,
                    tdl(),
                )
                .await;
        }

        assert_eq!(
            r.breakers
                .global_for(RendererKind::Lightpanda)
                .snapshot()
                .state,
            "closed",
            "one dead origin must not disable lightpanda for every other host"
        );
        assert_eq!(
            r.breakers
                .host_for("dead-origin.example", RendererKind::Lightpanda)
                .await
                .snapshot()
                .state,
            "open",
            "the host tier must still learn that this origin is unreachable"
        );
    }

    /// A skipped tier must report the budget the CALLER was given, never the
    /// sub-minimum remainder that is left when the skip fires.
    ///
    /// Regression guard for the production bug this pairs with: `remaining` is by
    /// definition below MIN_TIER_BUDGET on that branch and in production is ~0, so
    /// it rendered as the literal `Timeout after 1ms` on requests that had been
    /// granted 30s and had spent ~25s of it walking the ladder. The sibling test
    /// above only asserts the Timeout *variant*, so without this the exact bug can
    /// come back without failing anything.
    #[tokio::test]
    async fn budget_skip_reports_the_requested_budget_not_the_remainder() {
        let slow_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slow = Arc::new(SlowFailingFetcher {
            name: "lightpanda",
            burn: Duration::from_millis(1_200),
            calls: slow_calls.clone(),
        });
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let chrome = Arc::new(CountingFetcher {
            name: "camofox",
            calls: calls.clone(),
        });
        let r = make_renderer_with_mocks(vec![slow, chrome]);

        let err = r
            .fetch_with_js(
                "https://example.com",
                &HashMap::new(),
                None,
                None,
                crw_core::Deadline::from_request_ms(1_500),
            )
            .await
            .expect_err("both tiers must fail");

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "camofox must be skipped for lack of budget, or this test proves nothing"
        );
        match err {
            CrwError::Timeout(ms) => assert_eq!(
                ms, 1_500,
                "a budget skip must report the requested budget (1500ms), not the \
                 sub-minimum remainder; got {ms}ms"
            ),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    /// A tier that fails for a real reason, followed by a tier skipped for lack of
    /// budget, must still report Timeout (504) — not the earlier RendererError (500).
    /// Guards against reintroducing `get_or_insert_with` here.
    #[tokio::test]
    async fn budget_skip_overrides_an_earlier_renderer_error() {
        let slow_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slow = Arc::new(SlowFailingFetcher {
            name: "lightpanda",
            burn: Duration::from_millis(1_200),
            calls: slow_calls.clone(),
        });
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let chrome = Arc::new(CountingFetcher {
            name: "chrome",
            calls: calls.clone(),
        });
        let r = make_renderer_with_mocks(vec![slow, chrome]);

        // 1500ms budget: lightpanda is comfortably above the 500ms floor even under
        // CI scheduling jitter, burns 1200ms and errors, leaving ~300ms — chrome is
        // then below the floor and is skipped.
        let err = r
            .fetch_with_js(
                "https://example.com",
                &HashMap::new(),
                None,
                None,
                crw_core::Deadline::from_request_ms(1_500),
            )
            .await
            .expect_err("both tiers must fail");

        assert_eq!(
            slow_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the first tier must actually run, or this test proves nothing"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "chrome must be skipped for lack of budget"
        );
        assert!(
            matches!(err, CrwError::Timeout(_)),
            "a budget skip must overwrite the earlier RendererError so the server \
             still maps this to 504; got {err:?}"
        );
    }

    /// When the HTTP tier could not reach the origin at all, that error must win over
    /// the JS tier's generic RendererError. `TargetUnreachable` maps to 422 (the caller
    /// gave us a dead target); `RendererError` falls through to a 500 and reads as "our
    /// server broke".
    /// A 200-status vendor wall must tag the ladder as exhausted, exactly like a
    /// 403 one does.
    ///
    /// `crw_crawl::single` reads `JS_ESCALATION_FAILED` off the warning to decide
    /// whether the ladder is spent (`js_ladder_exhausted`). The tag used to be
    /// attached only on the `is_auth_blocked` arm, so a wall served under HTTP
    /// 200 — the canonical Turnstile shape — came back untagged and the whole
    /// ladder ran a SECOND time against a site that had just failed it, on a
    /// deadline it had already spent.
    #[tokio::test]
    async fn js_escalation_failure_tags_exhaustion_on_a_200_wall() {
        let js = Arc::new(MockFetcher {
            name: "camofox",
            behavior: MockBehavior::Err("Timeout after 5000ms".to_string()),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![js]);
        // HTTP 200 carrying a challenge shell: escalation fires via `is_blocked`,
        // NOT `is_auth_blocked` — the arm that never tagged.
        r.http = Arc::new(MockFetcher {
            name: "http",
            behavior: MockBehavior::OkStatus(
                200,
                "<html><head><title>Just a moment...</title></head><body>\
                 <div id=\"cf-browser-verification\"></div></body></html>"
                    .to_string(),
            ),
        });
        r.render_js_default = None; // auto branch

        let result = r
            .fetch(
                "https://walled.example",
                &HashMap::new(),
                None,
                None,
                None,
                tdl(),
            )
            .await
            .expect("falls back to the HTTP shell");

        let warning = result.warning.unwrap_or_default();
        assert!(
            warning.contains(JS_ESCALATION_FAILED),
            "a 200-status wall must report the ladder as exhausted so the caller \
             does not re-run it; got {warning:?}"
        );
    }

    /// An HTTP tier that cannot reach the origin at all.
    struct Unreachable;
    #[async_trait::async_trait]
    impl PageFetcher for Unreachable {
        async fn fetch(
            &self,
            url: &str,
            _h: &HashMap<String, String>,
            _w: Option<u64>,
            _d: crw_core::Deadline,
        ) -> CrwResult<FetchResult> {
            Err(CrwError::TargetUnreachable(format!(
                "Could not reach {url}"
            )))
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

    /// Prod shipped an anti-bot interstitial to a paying customer as
    /// `success: true` with `creditCost: 1`, because the ladder tail returned
    /// the best thin result without re-reading the verdict every tier had
    /// already reached. Live case: https://www.prlib.ru/en/history/619410 on
    /// 2026-09-03, "Security Check / Checking your browser", 95 chars.
    #[tokio::test]
    async fn wall_the_whole_ladder_failed_to_clear_is_not_a_success() {
        let wall = "<html><body><h1>Security Check</h1>\
                    <p>Checking your browser before accessing the site</p></body></html>";
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(wall.to_string()),
        }) as Arc<dyn PageFetcher>;
        let camofox = Arc::new(MockFetcher {
            name: "camofox",
            behavior: MockBehavior::Ok(wall.to_string()),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![lp, camofox]);
        // The HTTP tier cannot reach the origin, so the JS ladder actually runs
        // — the real shape of the prod case, where HTTP escalated and every JS
        // tier then hit the same wall.
        r.http = Arc::new(Unreachable);

        let out = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await;

        match out {
            Err(CrwError::HttpError(msg)) => {
                assert!(
                    msg.contains("anti-bot wall"),
                    "expected the wall to be named, got: {msg}"
                );
            }
            Ok(r) => panic!(
                "a wall no tier could clear must not ship as a success: rendered_with={:?} html={:?}",
                r.rendered_with, r.html
            ),
            Err(e) => panic!("expected HttpError naming the wall, got {e:?}"),
        }
    }

    /// The other half of the same gate: a page that is merely thin, with no
    /// wall phrasing, must still ship. Returning the best available body is the
    /// point of the ladder tail and the recall invariant rests on it.
    #[tokio::test]
    async fn thin_but_real_page_still_ships_from_the_ladder_tail() {
        let thin = "<html><body><h1>Notice</h1><p>Short but genuine page.</p></body></html>";
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(thin.to_string()),
        }) as Arc<dyn PageFetcher>;
        let camofox = Arc::new(MockFetcher {
            name: "camofox",
            behavior: MockBehavior::Ok(thin.to_string()),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![lp, camofox]);
        // The forced-JS arm fetches HTTP first for the content-type check, so
        // the real fetcher would answer before the ladder ever runs.
        r.http = Arc::new(MockFetcher {
            name: "http",
            behavior: MockBehavior::Ok(thin.to_string()),
        });

        let out = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await;
        assert!(
            out.is_ok(),
            "a thin but wall-free body must still be returned, got {:?}",
            out.err()
        );
    }

    /// The ladder correctly refuses a wall, and then the HTTP-shell fallback
    /// hands the same wall back anyway. Prod log, 2026-09-03: "JS escalation
    /// failed for soft-block status; surfacing HTTP shell with warning: ...
    /// blocked by an anti-bot wall that none of the 3 renderer tier(s)
    /// attempted could clear". The ladder's verdict has to survive that
    /// substitution, or rejecting the wall upstream buys nothing.
    #[tokio::test]
    async fn http_shell_fallback_does_not_resurrect_a_wall() {
        let wall = "<html><body><h1>Security Check</h1>\
                    <p>Checking your browser before accessing the site</p></body></html>";
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(wall.to_string()),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![lp]);
        r.http = Arc::new(MockFetcher {
            name: "http",
            behavior: MockBehavior::Ok(wall.to_string()),
        });

        let out = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await;
        assert!(
            out.is_err(),
            "the wall must not come back through the HTTP shell: {:?}",
            out.ok().map(|r| (r.rendered_with, r.html.len()))
        );
    }

    #[tokio::test]
    async fn unreachable_origin_beats_js_renderer_error() {
        let js = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Err("Navigation failed: net::ERR_SSL".to_string()),
        });
        let mut r = make_renderer_with_mocks(vec![js]);
        r.http = Arc::new(Unreachable);
        r.render_js_default = None; // auto branch

        let err = r
            .fetch(
                "https://dead.example",
                &HashMap::new(),
                None, // render_js: auto
                None, // wait_for_ms
                None, // requested_renderer
                tdl(),
            )
            .await
            .expect_err("both tiers fail");

        assert!(
            matches!(err, CrwError::TargetUnreachable(_)),
            "an unreachable origin must surface as TargetUnreachable (422), not the JS \
             tier's RendererError (500); got {err:?}"
        );
    }

    /// Control: with a healthy budget the same tier IS invoked. Guards against the
    /// floor silently disabling the ladder.
    #[tokio::test]
    async fn healthy_budget_still_invokes_js_tier() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock = Arc::new(CountingFetcher {
            name: "chrome",
            calls: calls.clone(),
        });
        let r = make_renderer_with_mocks(vec![mock]);

        let res = r
            .fetch_with_js("https://example.com", &HashMap::new(), None, None, tdl())
            .await
            .expect("healthy budget must render");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(res.html.contains("rendered"));
    }

    #[tokio::test]
    async fn fetch_with_pinned_renderer_filters_pool() {
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(rich_html("LP-")),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("chrome"),
                tdl(),
            )
            .await
            .unwrap();
        assert!(result.html.contains("CHROME-"), "expected chrome output");
        assert_eq!(result.rendered_with.as_deref(), Some("chrome"));
    }

    #[tokio::test]
    async fn fetch_with_pinned_renderer_unknown_returns_error() {
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![chrome]);

        let err = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("lightpanda"),
                tdl(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("lightpanda") && msg.contains("chrome"),
            "expected error to name pinned + available: {msg}"
        );
    }

    #[tokio::test]
    async fn fetch_with_renderer_auto_uses_full_chain() {
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(rich_html("LP-")),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("auto"),
                tdl(),
            )
            .await
            .unwrap();
        // First renderer in the chain wins when both succeed.
        assert!(result.html.contains("LP-"), "expected lightpanda first");
    }

    #[tokio::test]
    async fn failover_skips_renderer_that_returns_failed_render() {
        // LightPanda returns HTML with a Next.js error boundary marker.
        // The chain must skip it and use Chrome's healthy result.
        let bad_lp_html = format!(
            "<html><body><div id=\"__next-error-0\">{}</div></body></html>",
            "x".repeat(200)
        );
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(bad_lp_html),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-OK")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await
            .unwrap();
        assert!(result.html.contains("CHROME-OK"));
        assert_eq!(result.rendered_with.as_deref(), Some("chrome"));
    }

    #[tokio::test]
    async fn failover_surfaces_warning_when_only_failed_render_available() {
        // Only LightPanda is configured and it returns a failed render. The
        // call must succeed (best-effort thin_result fallback) but the warning
        // must name the failure so callers can surface it to the user.
        let bad_lp_html = format!(
            "<html><body><div id=\"__next-error-0\">{}</div></body></html>",
            "x".repeat(200)
        );
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(bad_lp_html),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await
            .unwrap();
        let warning = result.warning.expect("expected warning to be set");
        assert!(
            warning.contains("lightpanda") && warning.contains("nextjs_client_error"),
            "warning should name renderer + reason: {warning}"
        );
    }

    #[tokio::test]
    async fn failover_concats_warnings_across_two_failed_renderers() {
        // Both renderers return failed-render HTML. The fallback `thin_result`
        // should carry warnings from BOTH attempts so debugging captures the
        // full chain, not just the first failure.
        let bad_lp_html = format!(
            "<html><body><div id=\"__next-error-0\">{}</div></body></html>",
            "x".repeat(200)
        );
        let bad_chrome_html = format!(
            "<html><body><div id=\"__next_error__\">{}</div></body></html>",
            "y".repeat(200)
        );
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(bad_lp_html),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(bad_chrome_html),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await
            .unwrap();
        let warning = result.warning.expect("expected warning to be set");
        assert!(
            warning.contains("lightpanda") && warning.contains("chrome"),
            "warning should mention both renderers: {warning}"
        );
    }

    #[tokio::test]
    async fn fetch_pinned_renderer_failure_propagates() {
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Err("boom".into()),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![chrome]);
        // Stub the HTTP tier: the forced-JS arm fetches it before the ladder, so
        // without this the assertion depends on reaching example.com over the
        // network — and now that a JS failure can fall back to the HTTP body,
        // this test is the only thing pinning the hard-pin exclusion.
        r.http = Arc::new(MockFetcher {
            name: "http",
            behavior: MockBehavior::Ok(rich_html("HTTP-")),
        });

        let err = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("chrome"),
                tdl(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    /// `renderJs:true` used to be the only arm that threw away a perfectly good
    /// HTTP body when the JS ladder failed, so a forced-JS scrape returned a 504
    /// while holding the document.
    #[tokio::test]
    async fn forced_js_failure_falls_back_to_http_body() {
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Err("Timeout after 1ms".into()),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![chrome]);
        r.http = Arc::new(MockFetcher {
            name: "http",
            behavior: MockBehavior::Ok(rich_html("HTTP-")),
        });

        let res = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true), // forced JS
                None,
                None, // unpinned
                tdl(),
            )
            .await
            .expect("a failed JS ladder must not discard a valid HTTP body");
        assert!(res.html.contains("HTTP-"));
        // A 2xx fallback is the motivating case and the one most at risk of
        // going out silently: the caller asked for a browser, is billed either
        // way, and has nothing else in the response to tell them they got HTTP.
        assert!(
            res.warning
                .as_deref()
                .is_some_and(|w| w.contains("js_escalation_failed")),
            "every forced-JS fallback must be announced; got {:?}",
            res.warning
        );
        assert!(
            res.warning
                .as_deref()
                .is_some_and(|w| w.contains(JS_ESCALATION_FAILED)),
            "single.rs reads this exact prefix to skip re-escalation: {:?}",
            res.warning
        );
    }

    /// A 4xx/5xx body is usually an error shell, so the swap is surfaced rather
    /// than made silently — same rule the auto arm applies.
    #[tokio::test]
    async fn forced_js_failure_on_soft_block_warns() {
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Err("Timeout after 1ms".into()),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![chrome]);
        r.http = Arc::new(MockFetcher {
            name: "http",
            behavior: MockBehavior::OkStatus(403, rich_html("SHELL-")),
        });

        let res = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await
            .expect("soft-block bodies still ship, with a warning");
        assert!(
            res.warning
                .as_deref()
                .is_some_and(|w| w.contains("js_escalation_failed")),
            "the caller must be able to tell the JS tier failed; got {:?}",
            res.warning
        );
    }

    /// The real production failure is a deadline, and `MockBehavior::Err` can
    /// only build a `RendererError` — so the timeout shape gets its own fetcher.
    #[tokio::test]
    async fn forced_js_timeout_falls_back_to_http_body() {
        struct TimesOut;
        #[async_trait::async_trait]
        impl PageFetcher for TimesOut {
            async fn fetch(
                &self,
                _u: &str,
                _h: &HashMap<String, String>,
                _w: Option<u64>,
                _d: crw_core::Deadline,
            ) -> CrwResult<FetchResult> {
                Err(CrwError::Timeout(1))
            }
            fn name(&self) -> &str {
                "chrome"
            }
            fn supports_js(&self) -> bool {
                true
            }
            async fn is_available(&self) -> bool {
                true
            }
        }

        let mut r = make_renderer_with_mocks(vec![Arc::new(TimesOut)]);
        r.http = Arc::new(MockFetcher {
            name: "http",
            behavior: MockBehavior::Ok(rich_html("HTTP-")),
        });

        let res = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await
            .expect("a ladder timeout must not discard a valid HTTP body");
        assert!(res.html.contains("HTTP-"));
        assert_eq!(res.rendered_with.as_deref(), Some("http"));
    }

    #[tokio::test]
    async fn auto_promoted_host_tries_chrome_first() {
        // Pre-promote example.com via the preference learner so the loop
        // sorts chrome ahead of lightpanda even though lightpanda was
        // declared first. The first renderer in the executed order wins.
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(rich_html("LP-")),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        // Force-promote "example.com" by reaching the failure threshold.
        for _ in 0..3 {
            r.preferences
                .record_failure("example.com", &FailoverErrorKind::NextJsClientError)
                .await;
        }

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await
            .unwrap();
        assert!(
            result.html.contains("CHROME-"),
            "promoted host should hit chrome first, got: {}",
            &result.html[..80.min(result.html.len())]
        );
        assert_eq!(result.credit_cost, 2, "chrome costs 2 credits");
        assert!(matches!(
            result.render_decision,
            Some(RenderDecision::AutoPromoted {
                chosen: RendererKind::Chrome,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn breaker_skipped_renderer_falls_through_to_next() {
        // Trip the per-host breaker for lightpanda, then verify the loop
        // skips it and uses chrome — without ever calling lightpanda.fetch.
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Err("would fire if reached".into()),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-OK")),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![lp, chrome]);

        // Use a custom breaker config: long cooldown so the breaker can't
        // transition to half-open under parallel test load (the default
        // 5s cooldown was racing against scheduler latency on workspace runs).
        // Threshold/window stay tuned to default: 80 consecutive failures
        // satisfies min_calls=50 and far exceeds failure_rate=0.80.
        let breaker_cfg = BreakerConfig {
            base_cooldown: Duration::from_secs(300),
            max_cooldown: Duration::from_secs(300),
            ..BreakerConfig::default()
        };
        r.breakers = Arc::new(BreakerRegistry::new(breaker_cfg));
        for _ in 0..80 {
            r.breakers
                .record_result("example.com", RendererKind::Lightpanda, false)
                .await;
        }

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await
            .unwrap();
        assert!(result.html.contains("CHROME-OK"));
        assert!(matches!(
            result.render_decision,
            Some(RenderDecision::BreakerSkipped {
                skipped: RendererKind::Lightpanda,
                chosen: RendererKind::Chrome
            })
        ));
    }

    #[tokio::test]
    async fn user_pinned_failed_render_emits_warning() {
        // Pin lightpanda. It returns failed-render HTML (Next.js error
        // boundary). Because the user hard-pinned, no failover happens.
        // The thin result must carry an actionable warning so callers can
        // surface it instead of silently returning broken markdown.
        let bad_html = format!(
            "<html><body><div id=\"__next-error-0\">{}</div></body></html>",
            "x".repeat(200)
        );
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(bad_html),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("lightpanda"),
                tdl(),
            )
            .await
            .unwrap();
        let pin_hint = result
            .warnings
            .iter()
            .find(|w| w.starts_with("Pinned renderer 'lightpanda'"));
        assert!(
            pin_hint.is_some(),
            "expected pin-failure hint in warnings, got: {:?}",
            result.warnings
        );
        let hint = pin_hint.unwrap();
        assert!(
            hint.contains("nextJsClientError"),
            "hint should name camelCase reason: {hint}"
        );
        assert!(
            hint.contains("renderer=\"chrome\""),
            "hint should suggest a fix: {hint}"
        );
        // chain stays single-element because user pinned → no chrome attempt
        assert!(matches!(
            result.render_decision,
            Some(RenderDecision::Failover { ref chain, .. }) if chain.len() == 1
        ));
    }

    #[tokio::test]
    async fn user_pinned_decision_records_credit_and_kind() {
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![chrome]);
        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("chrome"),
                tdl(),
            )
            .await
            .unwrap();
        assert_eq!(result.credit_cost, 2);
        assert!(matches!(
            result.render_decision,
            Some(RenderDecision::UserPinned {
                renderer: RendererKind::Chrome
            })
        ));
    }

    #[tokio::test]
    async fn js_tier_escalates_on_403_status() {
        // LightPanda returns 403 with content (e.g. WAF block masked as content).
        // The chain must escalate to Chrome instead of accepting the 403 body.
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::OkStatus(403, rich_html("BLOCKED-")),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("auto"),
                tdl(),
            )
            .await
            .unwrap();
        assert!(
            result.html.contains("CHROME-"),
            "expected chrome output after lightpanda 403"
        );
        assert_eq!(result.status_code, 200);
    }

    #[tokio::test]
    async fn js_tier_escalates_on_vendor_block_with_200() {
        // LightPanda returns 200 with a Cloudflare challenge page. The chain
        // must escalate even though the status code is "successful".
        let cf_html = format!(
            "<html><head><script src=\"/cdn-cgi/challenge-platform/h/g/orchestrate/chl_page/v1\"></script></head><body>{}</body></html>",
            "x".repeat(200)
        );
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(cf_html),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("auto"),
                tdl(),
            )
            .await
            .unwrap();
        assert!(
            result.html.contains("CHROME-"),
            "expected chrome output after lightpanda vendor block"
        );
    }

    #[tokio::test]
    async fn js_tier_accepts_200_clean_response() {
        // Regression: a clean 200 from the first renderer must still be
        // accepted — no false escalation triggered by the new gates.
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(rich_html("LP-CLEAN-")),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("auto"),
                tdl(),
            )
            .await
            .unwrap();
        assert!(result.html.contains("LP-CLEAN-"));
        assert_eq!(result.status_code, 200);
    }

    /// A page the lightweight `detector` heuristics pass but the
    /// `crw_extract::antibot` classifier flags — a Reddit-class WAF block
    /// ("blocked by network security") served with HTTP 200.
    fn network_security_block_html() -> String {
        format!(
            "<html><body><article>You've been blocked by network security.{}</article></body></html>",
            "x".repeat(200)
        )
    }

    #[tokio::test]
    async fn js_tier_escalates_to_chrome_proxy_on_antibot_block() {
        // lightpanda + chrome both return a 200 WAF block the detector
        // misses; only the residential chrome_proxy tier clears it.
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(network_security_block_html()),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(network_security_block_html()),
        }) as Arc<dyn PageFetcher>;
        let chrome_proxy = Arc::new(MockFetcher {
            name: "chrome_proxy",
            behavior: MockBehavior::Ok(rich_html("PROXY-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome, chrome_proxy]);

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("auto"),
                tdl(),
            )
            .await
            .unwrap();
        assert!(
            result.html.contains("PROXY-"),
            "expected chrome_proxy output after antibot block"
        );
        assert_eq!(
            result.render_decision,
            Some(RenderDecision::Failover {
                chain: vec![
                    RendererKind::Lightpanda,
                    RendererKind::Chrome,
                    RendererKind::ChromeProxy,
                ],
                reason: FailoverErrorKind::AntibotBlock,
            })
        );
    }

    #[tokio::test]
    async fn antibot_block_returns_as_success_when_escalation_disabled() {
        // Kill switch: escalate_in_failover = false → classify() still runs
        // for telemetry, but the block page is returned as success with no
        // escalation. Proves the gate is wired correctly.
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(network_security_block_html()),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(rich_html("CHROME-")),
        }) as Arc<dyn PageFetcher>;
        let mut r = make_renderer_with_mocks(vec![lp, chrome]);
        r.antibot.escalate_in_failover = false;

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                Some("auto"),
                tdl(),
            )
            .await
            .unwrap();
        assert!(
            result.html.contains("network security"),
            "block page should be returned as-is when escalation is disabled"
        );
        assert_eq!(result.rendered_with.as_deref(), Some("lightpanda"));
    }

    #[tokio::test]
    async fn promoted_host_escalates_chrome_to_chrome_proxy_not_lightpanda() {
        // After host promotion the preference sort must place chrome_proxy
        // immediately after chrome — a chrome block escalates straight to
        // the residential tier, never back down to lightpanda.
        let lp = Arc::new(MockFetcher {
            name: "lightpanda",
            behavior: MockBehavior::Ok(rich_html("LP-")),
        }) as Arc<dyn PageFetcher>;
        let chrome = Arc::new(MockFetcher {
            name: "chrome",
            behavior: MockBehavior::Ok(network_security_block_html()),
        }) as Arc<dyn PageFetcher>;
        let chrome_proxy = Arc::new(MockFetcher {
            name: "chrome_proxy",
            behavior: MockBehavior::Ok(rich_html("PROXY-")),
        }) as Arc<dyn PageFetcher>;
        let r = make_renderer_with_mocks(vec![lp, chrome, chrome_proxy]);

        // Force-promote "example.com" so the loop sorts chrome first.
        for _ in 0..3 {
            r.preferences
                .record_failure("example.com", &FailoverErrorKind::NextJsClientError)
                .await;
        }

        let result = r
            .fetch(
                "https://example.com",
                &HashMap::new(),
                Some(true),
                None,
                None,
                tdl(),
            )
            .await
            .unwrap();
        assert!(
            result.html.contains("PROXY-"),
            "expected chrome_proxy output"
        );
        assert_eq!(
            result.render_decision,
            Some(RenderDecision::Failover {
                chain: vec![RendererKind::Chrome, RendererKind::ChromeProxy],
                reason: FailoverErrorKind::AntibotBlock,
            }),
            "chrome must escalate straight to chrome_proxy, skipping lightpanda"
        );
    }
}
