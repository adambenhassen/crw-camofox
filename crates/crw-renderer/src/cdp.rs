use async_trait::async_trait;
use crw_core::error::{CrwError, CrwResult};
use crw_core::types::{CapturedNetworkResponse, FetchResult};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore, broadcast};
use tokio_tungstenite::connect_async;

use crate::blocklist::{BlockReason, Blocklist};
use crate::cdp_conn::{CdpConnection, CdpEvent};
use crate::traits::PageFetcher;

/// Timeout for WebSocket connect handshake.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Extra overhead budget for the overall fetch timeout (on top of page_timeout + wait_for).
/// Covers WS connect, target create, navigate commit, snapshot eval, and cleanup.
pub const FETCH_OVERHEAD: Duration = Duration::from_secs(5);
/// Timeout for the Target.closeTarget cleanup command.
const TARGET_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum number of challenge retry attempts.
pub const CHALLENGE_MAX_RETRIES: u32 = 3;
/// Delay between challenge retry polls (ms).
pub const CHALLENGE_POLL_INTERVAL_MS: u64 = 3000;
/// Maximum time to poll for content stability when a loading placeholder
/// is detected after the initial wait.
pub const CONTENT_STABILITY_MAX_MS: u64 = 6000;
/// Interval between content-stability polls.
const CONTENT_STABILITY_TICK_MS: u64 = 500;
/// Max time to poll for a content selector before giving up and proceeding
/// with whatever HTML is currently rendered. Covers SPAs that hydrate after
/// `loadEventFired` (njcourts.gov, in-n-out.com, hangzhou customs, apploi).
pub const SPA_SELECTOR_MAX_MS: u64 = 8000;

/// Sum of per-tier CDP overhead in milliseconds — the difference between
/// `internal_timeout` (set by [`fetch_with_deadline`]) and the configured
/// per-tier `page_timeout`. Mirrored by `crw_core::config::CDP_TIER_OVERHEAD_MS`;
/// drift between the two is regression-tested in
/// `crates/crw-server/tests/cdp_constants_test.rs`.
pub const fn cdp_tier_overhead_ms() -> u64 {
    SPA_SELECTOR_MAX_MS
        + (CHALLENGE_MAX_RETRIES as u64) * CHALLENGE_POLL_INTERVAL_MS
        + CONTENT_STABILITY_MAX_MS
        + (FETCH_OVERHEAD.as_millis() as u64)
}
/// Interval between SPA selector polls.
const SPA_SELECTOR_TICK_MS: u64 = 200;
/// Body innerText length required before the SPA poll exits. Selectors mount
/// before content hydrates on apps like ticktick / happyhotel / qyxmbt /
/// smzdm — by waiting for body text to also pass this threshold, we avoid
/// snapshotting an empty shell that satisfies the selector check alone.
/// Pages with mostly-static content (nav, header, footer chrome) clear this
/// in the first poll tick; SPAs keep polling until hydration fills the body
/// or the budget elapses.
const SPA_BODY_TEXT_MIN_CHARS: u64 = 800;
/// Quiet-period for network-idle: number of ms with zero in-flight requests
/// before we consider the page "settled enough to snapshot". Mirrors
/// Playwright's `networkidle` (no requests for 500ms). XHR-driven SPAs that
/// finish their data fetch before innerText hits the threshold get an early
/// exit via this signal — recall lift on lazy-fetch pages.
const NETWORK_IDLE_QUIET_MS: i64 = 500;
/// Max iterations for the auto-scroll lazy-load pass. 12 viewports usually
/// covers infinite-scroll feeds without making it a crawl.
const AUTO_SCROLL_MAX_STEPS: u32 = 12;
/// Wait between each scroll step (ms). 250 balances giving lazy images
/// time to fire against total cost.
const AUTO_SCROLL_STEP_DELAY_MS: u64 = 250;
/// Hard ceiling on the entire auto-scroll phase. If we hit this, we
/// snapshot whatever's there and move on.
const AUTO_SCROLL_BUDGET_MS: u64 = 2500;
/// HTML size threshold above which auto-scroll is skipped. Pages this
/// big almost always have all their content already; the scroll pass
/// just adds latency and risks pushing us over the deadline.
const AUTO_SCROLL_HTML_SIZE_LIMIT: usize = 200_000;
/// Hard cap on the click-to-reveal pass. After this many clicks, stop —
/// any further reveals are diminishing returns and risk navigating away.
const AUTO_CLICK_MAX_CLICKS: u32 = 5;
/// Wait between clicks so each reveal can layout / hydrate.
const AUTO_CLICK_DELAY_MS: u64 = 250;
/// Hard ceiling on the entire click-to-reveal phase.
const AUTO_CLICK_BUDGET_MS: u64 = 1500;
/// Selector list checked when the caller didn't pass `wait_for_ms` — typical
/// SPA root containers. The first match wins.
const SPA_CONTENT_SELECTORS: &str = "main, article, [role=main], #content, #root > *, #app > *";

/// Maximum number of XHR/fetch responses captured for fallback extraction.
const NET_CAPTURE_MAX_BODIES: usize = 30;
/// Hard cap on cumulative body bytes captured per page.
const NET_CAPTURE_MAX_TOTAL_BYTES: usize = 2_000_000;
/// Minimum body size (Content-Length when known) to bother fetching.
const NET_CAPTURE_MIN_BODY_SIZE: usize = 512;
/// Per-getResponseBody command timeout.
const NET_CAPTURE_GETBODY_TIMEOUT: Duration = Duration::from_millis(800);

/// JavaScript injected via `Page.addScriptToEvaluateOnNewDocument` before every
/// navigation to prevent headless browser detection by anti-bot systems.
const STEALTH_JS: &str = r#"
// 1. Hide navigator.webdriver (primary headless signal for Cloudflare)
Object.defineProperty(navigator, 'webdriver', { get: () => false });

// 2. Fake chrome runtime object (missing in headless)
if (!window.chrome) {
    window.chrome = { runtime: {}, loadTimes: function(){}, csi: function(){} };
}

// 3. Spoof plugins array (headless has 0 plugins)
Object.defineProperty(navigator, 'plugins', {
    get: () => {
        const arr = [
            { name: 'Chrome PDF Plugin', filename: 'internal-pdf-viewer' },
            { name: 'Chrome PDF Viewer', filename: 'mhjfbmdgcfjbbpaeojofohoefgiehjai' },
            { name: 'Native Client', filename: 'internal-nacl-plugin' },
        ];
        arr.item = (i) => arr[i];
        arr.namedItem = (n) => arr.find(p => p.name === n);
        arr.refresh = () => {};
        return arr;
    }
});

// 4. Spoof languages (headless sometimes returns empty)
Object.defineProperty(navigator, 'languages', { get: () => ['en-US', 'en'] });

// 5. Override permissions query to hide "denied" for notifications
const originalQuery = window.navigator.permissions.query.bind(window.navigator.permissions);
window.navigator.permissions.query = (params) =>
    params.name === 'notifications'
        ? Promise.resolve({ state: Notification.permission })
        : originalQuery(params);

// 6. Prevent detection via iframe contentWindow
const origHTMLElement = HTMLIFrameElement.prototype.__lookupGetter__('contentWindow');
if (origHTMLElement) {
    Object.defineProperty(HTMLIFrameElement.prototype, 'contentWindow', {
        get: function() {
            const w = origHTMLElement.call(this);
            if (w && !w.chrome) w.chrome = window.chrome;
            return w;
        }
    });
}

// 7. Fix broken toString for overridden functions (anti-detection fingerprinting)
const nativeToString = Function.prototype.toString;
const overrides = new Map();
const proxy = new Proxy(nativeToString, {
    apply(target, thisArg, args) {
        const override = overrides.get(thisArg);
        return override || nativeToString.call(thisArg);
    }
});
Function.prototype.toString = proxy;
overrides.set(Function.prototype.toString, 'function toString() { [native code] }');

// 8. WebGL vendor/renderer spoof — anti-bot scripts inspect UNMASKED_VENDOR_WEBGL
// (37445) and UNMASKED_RENDERER_WEBGL (37446) to detect headless software rendering.
// Returning real GPU strings makes the browser look like a normal Windows desktop.
try {
    const getParameter = WebGLRenderingContext.prototype.getParameter;
    WebGLRenderingContext.prototype.getParameter = function(parameter) {
        if (parameter === 37445) return 'Intel Inc.';
        if (parameter === 37446) return 'Intel Iris OpenGL Engine';
        return getParameter.call(this, parameter);
    };
    if (typeof WebGL2RenderingContext !== 'undefined') {
        const getParameter2 = WebGL2RenderingContext.prototype.getParameter;
        WebGL2RenderingContext.prototype.getParameter = function(parameter) {
            if (parameter === 37445) return 'Intel Inc.';
            if (parameter === 37446) return 'Intel Iris OpenGL Engine';
            return getParameter2.call(this, parameter);
        };
    }
} catch (_) {}
"#;

/// One-shot consent banner / CMP dismissal. Runs once after page load,
/// before the SPA readiness poll, so the body innerText threshold doesn't
/// trip on banner text and the actual page content has a chance to hydrate
/// without an overlay swallowing focus. Ported subset of crawl4ai's
/// `js_snippet/remove_consent_popups.js`, restricted to the CMPs with
/// meaningful traffic share (OneTrust/CookiePro, Cookiebot, Usercentrics,
/// Sourcepoint, Quantcast, TrustArc, ConsentManager, TermsFeed) plus a
/// generic text-pattern fallback. Pierces open shadow roots and same-origin
/// iframes. Best-effort: every step is wrapped in try/catch and the snippet
/// returns the count of clicks made for telemetry only.
const CMP_DISMISS_JS: &str = r#"
(() => {
    let clicks = 0;
    const isVisible = (el) => {
        if (!el || !el.getBoundingClientRect) return false;
        const r = el.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) return false;
        const s = window.getComputedStyle(el);
        return s.display !== 'none' && s.visibility !== 'hidden' && s.opacity !== '0';
    };
    const click = (el) => {
        try {
            if (!isVisible(el)) return false;
            el.click();
            clicks++;
            return true;
        } catch (_) { return false; }
    };

    // CMP-specific accept selectors, ordered by deployment share. Only the
    // first match in each (hopefully exclusive) set is clicked.
    const SELECTORS = [
        '#onetrust-accept-btn-handler',
        '.ot-accept-all',
        '#CybotCookiebotDialogBodyButtonAccept',
        '#CybotCookiebotDialogBodyLevelButtonAccept',
        '[data-testid="uc-accept-all-button"]',
        '[data-cy="uc-accept-all-button"]',
        '.sp_choice_type_11',
        'button.message-component[title*="Accept" i]',
        '.qc-cmp2-summary-buttons button[mode="primary"]',
        '#qc-cmp2-ui button[mode="primary"]',
        '#truste-consent-button',
        '.cc-btn.cc-allow',
        '.cc-btn.cc-dismiss',
        'button[data-cmp-action="accept"]',
        'button[data-accept-action="all"]',
        'button[aria-label*="Accept all" i]',
        'button[aria-label*="Allow all" i]',
        '[id*="accept-cookies" i]',
        '[class*="accept-cookies" i]:not(input):not(textarea)',
    ];

    const tryRoot = (root) => {
        for (const sel of SELECTORS) {
            try {
                const el = root.querySelector(sel);
                if (el && click(el)) return;
            } catch (_) {}
        }
        // Generic text-match fallback: scan visible buttons for accept-ish copy.
        try {
            const buttons = root.querySelectorAll('button, [role="button"], input[type="button"], input[type="submit"]');
            const PATTERNS = /^(accept all|allow all|accept cookies|i accept|agree|got it|ok|tümünü kabul et|tout accepter|alle akzeptieren|aceptar todo)$/i;
            for (const b of buttons) {
                const t = (b.innerText || b.value || b.textContent || '').trim();
                if (PATTERNS.test(t) && click(b)) return;
            }
        } catch (_) {}
    };

    // Pass 1: light DOM
    tryRoot(document);

    // Pass 2: pierce open shadow roots one level deep (most CMPs flat).
    try {
        const all = document.querySelectorAll('*');
        for (const host of all) {
            if (host.shadowRoot) tryRoot(host.shadowRoot);
        }
    } catch (_) {}

    // Pass 3: same-origin iframes (Sourcepoint mounts inside iframe).
    try {
        for (const f of document.querySelectorAll('iframe')) {
            try {
                const doc = f.contentDocument || (f.contentWindow && f.contentWindow.document);
                if (doc) tryRoot(doc);
            } catch (_) {}
        }
    } catch (_) {}

    // Pass 4: IAB TCF v2 — programmatic opt-in if API present.
    try {
        if (typeof window.__tcfapi === 'function') {
            window.__tcfapi('ping', 2, (data, ok) => {
                if (ok && data && data.cmpStatus !== 'error') {
                    try { window.__tcfapi('addEventListener', 2, () => {}); } catch (_) {}
                }
            });
        }
    } catch (_) {}

    return clicks;
})()
"#;

/// HTML snapshot expression. Fast path returns `document.documentElement
/// .outerHTML` directly. When any element exposes an open shadow root, we
/// switch to a recursive serializer that resolves `<slot>` projections into
/// the light DOM and skips shadow-scoped `<style>` (those are only
/// meaningful inside the shadow tree). Ported from crawl4ai's
/// `js_snippet/flatten_shadow_dom.js` so web-component-driven sites
/// (Shoelace, Material Web, custom-element CMSes) surface their content in
/// the markdown extractor instead of producing an empty shell.
const HTML_SNAPSHOT_JS: &str = r#"
(() => {
    const VOID = new Set([
        'area','base','br','col','embed','hr','img','input',
        'link','meta','param','source','track','wbr'
    ]);
    let hasShadow = false;
    try {
        const all = document.querySelectorAll('*');
        for (let i = 0; i < all.length; i++) {
            if (all[i].shadowRoot) { hasShadow = true; break; }
        }
    } catch (_) {}
    if (!hasShadow) return document.documentElement.outerHTML;

    const escAttr = (v) => String(v).replace(/&/g, '&amp;').replace(/"/g, '&quot;');
    const serializeAttrs = (node) => {
        let s = '';
        for (const a of node.attributes || []) {
            s += ` ${a.name}="${escAttr(a.value)}"`;
        }
        return s;
    };

    const serialize = (node) => {
        if (node.nodeType === Node.TEXT_NODE) return node.textContent;
        if (node.nodeType === Node.COMMENT_NODE) return '';
        if (node.nodeType !== Node.ELEMENT_NODE) return '';
        const tag = node.tagName.toLowerCase();
        const attrs = serializeAttrs(node);
        let inner = '';
        if (node.shadowRoot) {
            inner = serializeShadowRoot(node);
        } else {
            for (const child of node.childNodes) inner += serialize(child);
        }
        if (VOID.has(tag)) return `<${tag}${attrs}>`;
        return `<${tag}${attrs}>${inner}</${tag}>`;
    };

    const serializeShadowRoot = (host) => {
        let result = '';
        for (const child of host.shadowRoot.childNodes) {
            result += serializeShadowChild(child, host);
        }
        return result;
    };

    const serializeShadowChild = (node, host) => {
        if (node.nodeType === Node.TEXT_NODE) return node.textContent;
        if (node.nodeType === Node.COMMENT_NODE) return '';
        if (node.nodeType !== Node.ELEMENT_NODE) return '';
        const tag = node.tagName.toLowerCase();
        if (tag === 'style') return '';
        if (tag === 'slot') {
            const assigned = node.assignedNodes({ flatten: true });
            if (assigned.length > 0) {
                let out = '';
                for (const a of assigned) out += serialize(a);
                return out;
            }
            let fallback = '';
            for (const child of node.childNodes) {
                fallback += serializeShadowChild(child, host);
            }
            return fallback;
        }
        const attrs = serializeAttrs(node);
        let inner = '';
        if (node.shadowRoot) {
            inner = serializeShadowRoot(node);
        } else {
            for (const child of node.childNodes) {
                inner += serializeShadowChild(child, host);
            }
        }
        if (VOID.has(tag)) return `<${tag}${attrs}>`;
        return `<${tag}${attrs}>${inner}</${tag}>`;
    };

    return serialize(document.documentElement);
})()
"#;

/// Lightweight CDP client that talks directly to any CDP-compatible browser
/// (LightPanda, Chrome, Playwright) via WebSocket.
///
/// Uses a semaphore to limit concurrent connections to `pool_size`,
/// preventing connection storms under heavy concurrent crawl loads.
pub struct CdpRenderer {
    name: String,
    /// Base WS URL from config (e.g. "ws://chrome:9222/").
    /// For Chrome/Chromium, the actual browser WS URL includes a dynamic ID
    /// (e.g. "ws://chrome:9222/devtools/browser/<uuid>") and must be discovered
    /// at runtime via the /json/version HTTP endpoint.
    configured_ws_url: String,
    /// Lazily resolved browser-level WS URL (discovered from /json/version).
    /// Wrapped in `Mutex<Option<...>>` rather than `OnceCell` so we can
    /// invalidate on CDP connect failure: chrome restarts mint a new
    /// `/devtools/browser/<uuid>` path, and a stale cached value would dial
    /// a dead URL forever until process restart. See `invalidate_resolved_ws_url`.
    resolved_ws_url: Arc<StdMutex<Option<String>>>,
    page_timeout: Duration,
    /// Hard ceiling on the post-navigate wait+snapshot+stability+challenge
    /// phase. Wraps the work in a budget race; on hit the renderer snapshots
    /// whatever DOM is present and flags `truncated = true`.
    nav_budget: Duration,
    /// Whether to enable `Fetch.requestPaused` interception for chrome tier.
    /// When true, the pump runs alongside navigate and blocks requests per
    /// `blocklist`. Off-by-default per Phase 2 plan; flipped via config.
    intercept_enabled: bool,
    blocklist: Blocklist,
    /// Host substrings (case-insensitive) for which interception is force-disabled
    /// even when `intercept_enabled = true`.
    host_intercept_disable: Vec<String>,
    conn_semaphore: Arc<Semaphore>,
    /// Browser context pool. `Some` when `[renderer.chrome.pool] enabled = true`
    /// AND backend is vanilla chrome (not browserless v2 — gated off in v1
    /// per plan §"Out of scope"). When `Some`, `fetch` dispatches through
    /// `fetch_with_pool`; when `None`, legacy `fetch_with_ws` is used.
    pool: Option<Arc<crate::browser_pool::BrowserContextPool<CdpConnection>>>,
    /// DataImpulse base credentials (username without country suffix, password).
    /// When `Some`, the renderer drives Chrome's proxy auth via CDP
    /// `Fetch.authRequired`, composing the country-suffixed username per request.
    /// Only the chrome_proxy tier sets this; plain chrome leaves it `None`.
    proxy_auth_base: Option<(String, String)>,
    /// Country code used when a `ScrapeRequest.country` is not supplied.
    /// `None` means "no suffix" → DataImpulse global pool.
    default_country: Option<String>,
    /// UA for `Network.setUserAgentOverride`; empty = no override (browser default).
    user_agent: String,
}

impl CdpRenderer {
    pub fn new(name: &str, ws_url: &str, page_timeout_ms: u64, pool_size: usize) -> Self {
        let pool_size = pool_size.max(1);
        let page_timeout = Duration::from_millis(page_timeout_ms);
        Self {
            name: name.to_string(),
            configured_ws_url: ws_url.to_string(),
            resolved_ws_url: Arc::new(StdMutex::new(None)),
            page_timeout,
            nav_budget: page_timeout,
            intercept_enabled: false,
            blocklist: Blocklist::defaults(),
            host_intercept_disable: Vec::new(),
            conn_semaphore: Arc::new(Semaphore::new(pool_size)),
            pool: None,
            proxy_auth_base: None,
            default_country: None,
            user_agent: String::new(),
        }
    }

    /// Set the User-Agent the CDP renderer presents (via
    /// `Network.setUserAgentOverride`). Pass the same `effective_ua` the HTTP
    /// fetcher uses so a JS-rendered page sees a modern UA, not the browser's
    /// default — and HTTP/CDP UAs match (a mismatch is itself a bot tell).
    pub fn with_user_agent(mut self, ua: &str) -> Self {
        self.user_agent = ua.to_string();
        self
    }

    /// Configure DataImpulse base proxy credentials. The `Fetch.authRequired`
    /// pump composes the per-request username as `{base_user}__cr.{country}`,
    /// resolved from `RequestContext::country` (set via `REQUEST_COUNTRY` task-local)
    /// with `default_country` as the fallback when the request omits it.
    pub fn with_proxy_auth_base(
        mut self,
        base_user: String,
        base_pass: String,
        default_country: Option<String>,
    ) -> Self {
        self.proxy_auth_base = Some((base_user, base_pass));
        self.default_country = default_country;
        self
    }

    /// Enable the browser-context pool. Builds an `Arc<BrowserContextPool>`
    /// whose factory calls back into the same connect-with-retry path as the
    /// legacy `fetch_with_ws` (preserves the cached-WS-URL invalidation from
    /// commit `b5f7bec`).
    pub fn with_pool(mut self, cfg: crate::browser_pool::PoolCfg) -> Self {
        let name = self.name.clone();
        let configured = self.configured_ws_url.clone();
        let resolved_cache = self.resolved_ws_url.clone();
        let page_timeout = self.page_timeout;
        let factory: crate::browser_pool::ConnFactory<CdpConnection> = Arc::new(move || {
            let name = name.clone();
            let configured = configured.clone();
            let resolved_cache = resolved_cache.clone();
            Box::pin(async move {
                let conn =
                    connect_chrome_with_retry(&name, &configured, &resolved_cache, page_timeout)
                        .await?;
                Ok(Arc::new(conn))
            })
        });
        crw_core::metrics::metrics()
            .chrome_pool_size
            .set(cfg.size as i64);
        self.pool = Some(crate::browser_pool::BrowserContextPool::new(cfg, factory));
        self
    }

    pub fn pool(&self) -> Option<Arc<crate::browser_pool::BrowserContextPool<CdpConnection>>> {
        self.pool.clone()
    }

    /// Override the post-navigate budget. Default equals `page_timeout_ms`.
    /// Set from `RendererConfig::chrome_nav_budget_ms` for the chrome tier.
    pub fn with_nav_budget(mut self, nav_budget_ms: u64) -> Self {
        self.nav_budget = Duration::from_millis(nav_budget_ms);
        self
    }

    /// Enable `Fetch.requestPaused` interception driven by `blocklist`.
    /// `host_disable` is a list of host substrings that opt out per-request.
    pub fn with_interception(
        mut self,
        enabled: bool,
        blocklist: Blocklist,
        host_disable: Vec<String>,
    ) -> Self {
        self.intercept_enabled = enabled;
        self.blocklist = blocklist;
        self.host_intercept_disable = host_disable.iter().map(|s| s.to_lowercase()).collect();
        self
    }

    /// `true` if interception is configured-on AND the URL's host is not on
    /// the per-host opt-out list.
    fn intercept_active_for(&self, url: &str) -> bool {
        if !self.intercept_enabled {
            return false;
        }
        if self.host_intercept_disable.is_empty() {
            return true;
        }
        let host = match url::Url::parse(url) {
            Ok(u) => u.host_str().map(|s| s.to_lowercase()).unwrap_or_default(),
            Err(_) => return true,
        };
        !self.host_intercept_disable.iter().any(|h| host.contains(h))
    }
}

/// Resolve the actual browser WebSocket URL, caching the result. Free
/// function so both `CdpRenderer::resolve_ws_url` and the pool's connect
/// factory share a single implementation with shared cache invalidation
/// semantics.
async fn resolve_ws_url_with_cache(
    configured: &str,
    cache: &StdMutex<Option<String>>,
    _page_timeout: Duration,
) -> CrwResult<String> {
    if let Some(cached) = cache.lock().unwrap().clone() {
        return Ok(cached);
    }

    let resolved = if configured.contains("/devtools/") || is_browserless_direct_ws(configured) {
        // Already-resolved /devtools/ URL OR browserless v2 / commercial CDP
        // endpoint that serves a WS directly (no /json/version).
        configured.to_string()
    } else if let Ok(Ok((ws, _))) =
        tokio::time::timeout(Duration::from_secs(3), connect_async(configured)).await
    {
        drop(ws);
        configured.to_string()
    } else {
        let http_url = configured
            .replace("ws://", "http://")
            .replace("wss://", "https://")
            .trim_end_matches('/')
            .to_string()
            + "/json/version";

        tracing::info!("Discovering browser WS URL from {http_url}");

        let resp = reqwest::Client::new()
            .get(&http_url)
            .header("Host", "localhost")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| {
                // The endpoint is already in the `Discovering browser WS URL` line
                // above; it is internal infrastructure and must not reach the caller.
                CrwError::RendererError(format!(
                    "CDP discovery failed: {}",
                    crw_core::error::reqwest_message(e)
                ))
            })?;

        let body: serde_json::Value = resp.json().await.map_err(|e| {
            CrwError::RendererError(format!(
                "CDP discovery parse error: {}",
                crw_core::error::reqwest_message(e)
            ))
        })?;

        let ws_url = body
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CrwError::RendererError("No webSocketDebuggerUrl in /json/version".into())
            })?;

        let rewritten = rewrite_ws_host(ws_url, configured);
        tracing::info!(ws_url = %rewritten, "Discovered browser WS URL");
        rewritten
    };

    *cache.lock().unwrap() = Some(resolved.clone());
    Ok(resolved)
}

impl CdpRenderer {
    /// Resolve the WS URL and open a CDP connection. On failure, invalidate
    /// the cached URL and retry once — covers the chrome-restart case where
    /// the cached `/devtools/browser/<uuid>` is stale.
    async fn connect_with_retry(&self) -> CrwResult<CdpConnection> {
        connect_chrome_with_retry(
            &self.name,
            &self.configured_ws_url,
            &self.resolved_ws_url,
            self.page_timeout,
        )
        .await
    }
}

/// Connect to a Chrome CDP endpoint with cached-WS-URL invalidation on first
/// failure. Shared between `CdpRenderer::connect_with_retry` (legacy path)
/// and `BrowserContextPool`'s factory (preserves the b5f7bec invalidation
/// guarantee — chrome restarts mint a fresh `/devtools/browser/<uuid>`,
/// and the cache must be drop-and-rebuild on connect failure).
async fn connect_chrome_with_retry(
    name: &str,
    configured_ws_url: &str,
    resolved_cache: &StdMutex<Option<String>>,
    page_timeout: Duration,
) -> CrwResult<CdpConnection> {
    let t0 = Instant::now();
    let result =
        connect_chrome_with_retry_inner(name, configured_ws_url, resolved_cache, page_timeout)
            .await;
    let outcome = classify_connect_outcome(&result);
    crw_core::metrics::metrics()
        .chrome_connect_seconds
        .with_label_values(&[outcome])
        .observe(t0.elapsed().as_secs_f64());
    result
}

/// Strip "Mozilla/5.0 " so lightpanda accepts the UA — its `validateUserAgent`
/// rejects any "Mozilla" (`error.Reserved`). The "...Chrome/<ver>..." token survives.
fn lightpanda_safe_ua(ua: &str) -> &str {
    ua.strip_prefix("Mozilla/5.0 ").unwrap_or(ua)
}

async fn connect_chrome_with_retry_inner(
    name: &str,
    configured_ws_url: &str,
    resolved_cache: &StdMutex<Option<String>>,
    page_timeout: Duration,
) -> CrwResult<CdpConnection> {
    let ws_url = resolve_ws_url_with_cache(configured_ws_url, resolved_cache, page_timeout).await?;
    match CdpConnection::connect(&ws_url, WS_CONNECT_TIMEOUT).await {
        Ok(conn) => Ok(conn),
        Err(e) => {
            tracing::warn!(
                renderer = name,
                error = %e,
                "CDP connect failed; invalidating cached ws_url and retrying once"
            );
            *resolved_cache.lock().unwrap() = None;
            let ws_url =
                resolve_ws_url_with_cache(configured_ws_url, resolved_cache, page_timeout).await?;
            CdpConnection::connect(&ws_url, WS_CONNECT_TIMEOUT).await
        }
    }
}

/// Bucket a `connect_with_retry` result into one of the Tier 0 outcome labels:
/// `ok`, `ws_handshake_timeout`, `version_probe_fail`, `ws_dial_fail`.
fn classify_connect_outcome(r: &CrwResult<CdpConnection>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(CrwError::Timeout(_)) => "ws_handshake_timeout",
        Err(CrwError::RendererError(msg)) if msg.contains("CDP discovery") => "version_probe_fail",
        Err(_) => "ws_dial_fail",
    }
}

/// Recognise commercial / browserless-style CDP endpoints that serve a
/// WebSocket directly and don't expose `/json/version`. Such URLs
/// either carry a `token=` query parameter or use a browser-named path
/// (`/chromium`, `/firefox`, `/webkit`).
fn is_browserless_direct_ws(url: &str) -> bool {
    if url.contains("token=") {
        return true;
    }
    url.contains("/chromium") || url.contains("/firefox") || url.contains("/webkit")
}

/// Rewrite the host:port of a WS URL to match the configured endpoint.
/// Chrome's /json/version returns "ws://127.0.0.1:9222/devtools/browser/..." but
/// from another container we need "ws://chrome:9222/devtools/browser/...".
fn rewrite_ws_host(discovered: &str, configured: &str) -> String {
    let conf_stripped = configured
        .trim_start_matches("ws://")
        .trim_start_matches("wss://");
    let conf_host_port = conf_stripped.split('/').next().unwrap_or(conf_stripped);

    let disc_stripped = discovered
        .trim_start_matches("ws://")
        .trim_start_matches("wss://");
    let disc_path = disc_stripped
        .find('/')
        .map(|i| &disc_stripped[i..])
        .unwrap_or("/");

    let scheme = if configured.starts_with("wss://") {
        "wss://"
    } else {
        "ws://"
    };
    format!("{scheme}{conf_host_port}{disc_path}")
}

/// Build the `Fetch.continueWithAuth` payload. Pure fn — testable without a
/// live CDP connection. When `creds` is `Some`, replies with
/// `ProvideCredentials`; when `None`, replies with `CancelAuth` (NOT `Default`,
/// which is ambiguous in headless and may pop a dialog in headed mode).
fn build_auth_response(request_id: &str, creds: Option<(&str, &str)>) -> serde_json::Value {
    match creds {
        Some((user, pass)) => serde_json::json!({
            "requestId": request_id,
            "authChallengeResponse": {
                "response": "ProvideCredentials",
                "username": user,
                "password": pass,
            },
        }),
        None => serde_json::json!({
            "requestId": request_id,
            "authChallengeResponse": { "response": "CancelAuth" },
        }),
    }
}

/// Drive `Fetch.authRequired` events to supply DataImpulse credentials per
/// request. Mirrors `run_intercept_pump`'s shape (borrow `&CdpConnection`,
/// filter by `session_id`, exit on `Closed`).
///
/// `creds` is composed *per `fetch_inner` call* with the request's country
/// suffix already applied, captured by move into this future so concurrent
/// pool slots cannot cross-contaminate credentials.
///
/// This pump answers `Fetch.authRequired` only. `Fetch.requestPaused` is owned
/// by [`run_intercept_pump`], which now runs on every navigation, so answering
/// paused requests here too would double-continue them.
async fn run_auth_pump(
    conn: &CdpConnection,
    mut rx: broadcast::Receiver<CdpEvent>,
    creds: Option<(String, String)>,
    session_id: &str,
) {
    let cmd_timeout = Duration::from_secs(2);
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if ev.session_id.as_deref() != Some(session_id) {
            continue;
        }
        if ev.method != "Fetch.authRequired" {
            continue;
        }
        let request_id = ev
            .params
            .get("requestId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if request_id.is_empty() {
            continue;
        }
        let creds_ref = creds.as_ref().map(|(u, p)| (u.as_str(), p.as_str()));
        let payload = build_auth_response(&request_id, creds_ref);
        let _ = conn
            .send_recv(
                "Fetch.continueWithAuth",
                payload,
                Some(session_id),
                cmd_timeout,
            )
            .await;
    }
}

/// Why the browser may not make this request, or `None` to allow it.
///
/// The route layer validates only the URL the caller supplied. Everything the
/// page does afterwards — server redirects, `<meta refresh>`, JS navigation,
/// same-process iframes, XHR and `fetch` — is issued by the browser's own
/// network stack, so an interception handler is the only point where those
/// destinations can be checked. This is the CDP equivalent of what
/// [`crw_core::url_safety::safe_redirect_policy`] already does for the
/// HTTP-only tier.
async fn outbound_block_label(req_url: &str, ctx: &OutboundCtx) -> Option<&'static str> {
    let Ok(parsed) = url::Url::parse(req_url) else {
        // Chrome sends absolute, normalised URLs; anything we cannot parse is
        // anomalous, so fail closed.
        return Some(BLOCK_LABEL_POLICY);
    };
    // Schemes that never reach a socket. Failing them would break ordinary
    // pages for no gain. An allowlist, not "anything that is not http(s)":
    // `validate_safe_host` deliberately dropped the scheme rule, so this is the
    // only scheme gate left on this path.
    if matches!(parsed.scheme(), "data" | "blob" | "about") {
        return None;
    }
    // Cheap first: literal addresses and the hostname deny list, no DNS.
    //
    // `validate_safe_host`, not `validate_safe_url`: the URL-shape rules
    // (2048-char cap, scheme) belong at the route layer, where the caller chose
    // the URL. A signed CDN subresource past the cap is not an SSRF, and
    // failing it here would silently drop a legitimate request.
    if !matches!(parsed.scheme(), "http" | "https")
        || crw_core::url_safety::validate_safe_host(&parsed).is_err()
    {
        return Some(BLOCK_LABEL_POLICY);
    }
    let Some(port) = parsed.port_or_known_default() else {
        return Some(BLOCK_LABEL_POLICY);
    };
    let key = format!("{}:{port}", parsed.host_str().unwrap_or_default());
    // Per-render memo, not a process-wide cache with a TTL. One page emits many
    // requests to a handful of hosts (a github.com navigation pauses 14 requests
    // across 2 hosts), and re-resolving each one queues behind `RESOLVE_LIMIT`
    // for no benefit — the browser is reusing its own cached address anyway. The
    // rebinding window this opens is one page load, not a cache lifetime, which
    // is why a shared TTL cache was rejected.
    //
    // Only allow verdicts are memoised. A denial can come from a transient
    // resolver failure (SERVFAIL under load is the common failure mode, not a
    // stall), and latching that would block a legitimate host for the rest of
    // the render with no retry.
    if ctx.memo.lock().ok().and_then(|m| m.get(&key).copied()) == Some(true) {
        return None;
    }
    // Bounded end to end. `RESOLVE_LIMIT` is process-wide and FIFO-fair, so an
    // unbounded wait here would be a hang one layer below the pump: the handler
    // never answers its paused request and the page stalls to the nav budget.
    // Fails closed on expiry, which costs one subresource rather than the page.
    //
    // The budget is the tier's own navigation timeout, not a fixed constant. A
    // verdict that arrives after `Page.navigate` has already timed out cannot
    // help anyone: on the LightPanda tier that ceiling is 2.5s, so a longer
    // budget only converts renders that would have succeeded into failures.
    let classified = tokio::time::timeout(ctx.budget, async {
        // Two gates: a per-render one so a single page with hundreds of unique
        // hosts cannot occupy the whole process-wide pool and force fail-closed
        // denials in every other concurrent render, and the global one that
        // bounds total resolver load.
        let _render_permit = ctx.render_limit.acquire().await;
        let _permit = RESOLVE_LIMIT.acquire().await;
        crw_core::url_safety::classify_safe_host_resolved(&parsed).await
    })
    .await;
    match classified {
        Ok(Ok(())) => {
            if let Ok(mut m) = ctx.memo.lock() {
                m.insert(key, true);
            }
            None
        }
        // A destination that resolved into a blocked range is a policy call and
        // stays labelled as one. Reporting it as "we could not check" would hide
        // the thing this guard exists to surface — a rebinding attempt is public
        // at the route layer and internal by the time the browser asks — and
        // would route it into the caller-refund path below.
        Ok(Err(crw_core::url_safety::HostRejection::Policy(_))) => Some(BLOCK_LABEL_POLICY),
        // We could not establish where this points: resolver error, or the check
        // ran out of budget. That is our failure, not the caller's, so record it
        // and let the caller-facing error say so rather than blaming the origin:
        // both tiers share one resolver, so a brown-out otherwise reads as "the
        // target is unreachable", gets a 422 and a refund, and never trips the
        // 5xx watchdog.
        //
        // Scoped to the navigated origin. A parked ad or analytics host that no
        // longer resolves is common, and letting it relabel the render would
        // deny a refund for an origin the caller really could not reach.
        Ok(Err(crw_core::url_safety::HostRejection::Unresolved(_))) | Err(_) => {
            if parsed.host_str().is_some_and(|h| h == ctx.doc_host) {
                ctx.unresolved
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Some(BLOCK_LABEL_UNRESOLVED)
        }
    }
}

/// Per-render state the destination check needs.
struct OutboundCtx {
    memo: ResolveMemo,
    /// One render's share of [`RESOLVE_LIMIT`].
    render_limit: tokio::sync::Semaphore,
    /// Ceiling on one check, taken from the tier's navigation timeout.
    budget: Duration,
    /// Host of the URL being navigated. Only this host's failures may relabel
    /// the render as our own failure.
    doc_host: String,
    /// Set when a check failed because our own resolver could not answer.
    unresolved: std::sync::atomic::AtomicBool,
}

/// Host verdicts already decided during one render. See [`outbound_allowed`].
type ResolveMemo = Arc<StdMutex<HashMap<String, bool>>>;

/// Paused requests the pump has not answered yet, as `(session id, request id)`.
type Outstanding = Arc<StdMutex<std::collections::HashSet<(String, String)>>>;

/// The destination is inside a range or on a host we refuse to reach.
const BLOCK_LABEL_POLICY: &str = "outbound";
/// The destination could not be proved safe: resolver error or budget expiry.
const BLOCK_LABEL_UNRESOLVED: &str = "outbound_unresolved";

/// One render's share of [`RESOLVE_LIMIT`]. Small enough that a page with
/// hundreds of unique hosts cannot park the whole global pool, large enough
/// that an ordinary page's handful of distinct hosts never queues.
const PER_RENDER_RESOLVE_LIMIT: usize = 8;

/// Process-wide ceiling on concurrent destination lookups.
///
/// Every render on every tier now pauses every request, so without a global
/// bound a few hundred concurrent scrapes would put an unbounded number of
/// `getaddrinfo` calls on the shared blocking pool, which HTML extraction and
/// PDF parsing also use. Bounding here rather than per pump keeps the receiver
/// loop free to keep draining CDP events: a pump that stops reading its
/// broadcast channel starts dropping `Fetch.requestPaused` events, and a
/// dropped one is never answered.
static RESOLVE_LIMIT: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(256));

/// Answer every `Fetch.requestPaused` event: validate the destination, apply
/// the ad/resource blocklist when one is configured, then continue or fail
/// (`BlockedByClient`). Runs forever until cancelled (the future is dropped
/// when the work future completes inside `tokio::select!`).
///
/// `blocklist` is `None` when resource interception is configured off for this
/// URL; the destination check still runs, because it is what keeps the browser
/// from being driven into the operator's own network.
///
/// Concurrency: handlers run in a bounded `FuturesUnordered` rather than inline.
/// The destination check can cost a DNS lookup, and the event receiver must not
/// block on it: the broadcast ring is shared with every other CDP event, and a
/// `Lagged` drop of a `Fetch.requestPaused` is unrecoverable — that request is
/// never answered and hangs until the navigation budget expires. Saturation
/// applies backpressure instead of failing requests; answering our own
/// saturation with `failRequest` would drop real subresources and surface as a
/// badly rendered page rather than as an error.
async fn run_intercept_pump(
    conn: &CdpConnection,
    mut rx: broadcast::Receiver<CdpEvent>,
    blocklist: Option<&Blocklist>,
    session_id: &str,
    ctx: &OutboundCtx,
    outstanding: &Outstanding,
) {
    use futures::future::{BoxFuture, FutureExt};
    use futures::stream::{FuturesUnordered, StreamExt};

    let cmd_timeout = Duration::from_secs(2);
    // Boxed because two differently-shaped handlers share the set: enabling
    // interception on a newly attached child target, and answering a paused
    // request.
    let mut in_flight: FuturesUnordered<BoxFuture<'_, ()>> = FuturesUnordered::new();
    // Child sessions auto-attached under our page target (out-of-process
    // iframes, workers). Their requests carry their own session id, so they
    // have to be recognised as ours, and only ours: answering a session we did
    // not attach risks double-continuing a request another consumer owns.
    let mut child_sessions: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        // No hard in-flight cap: a cap that makes this loop stop calling
        // `rx.recv()` is how `Fetch.requestPaused` events get dropped, and a
        // dropped one hangs its request until the navigation budget expires.
        // Resolver load is bounded globally by `RESOLVE_LIMIT` instead.
        //
        // Deliberately not `biased`: preferring the receiver would let a busy
        // event stream starve the handlers, and a handler that never runs is a
        // request that is never answered. Fair polling keeps both moving. An
        // empty set yields `None`, which disables that branch.
        let received = tokio::select! {
            ev = rx.recv() => ev,
            Some(()) = in_flight.next() => continue,
        };
        let ev = match received {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                // Neither loss is recoverable here. A missed
                // `Fetch.requestPaused` leaves that request paused until the
                // target closes, which is fail-closed but costs the subresource;
                // a missed `Target.attachedToTarget` leaves that child paused for
                // the rest of the render, because nothing else sends
                // `Runtime.runIfWaitingForDebugger`. Make it visible rather than
                // silent.
                tracing::warn!(
                    dropped,
                    "CDP event backlog overflowed; a paused request or child target may stall"
                );
                crw_core::metrics::metrics()
                    .chrome_blocked_requests_total
                    .with_label_values(&["event_lag"])
                    .inc();
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        // A child target attached under our page: enable interception on it too,
        // otherwise its requests never pause. This is what covers an
        // out-of-process iframe, whose navigation belongs to its own target and
        // would otherwise render into a screenshot unchecked. Browser-scope
        // targets (service workers) still do not attach to a page session and
        // remain outside this; on LightPanda they are covered by
        // `--block-private-networks` instead.
        let attach_parent_is_ours = ev
            .session_id
            .as_deref()
            .is_some_and(|s| s == session_id || child_sessions.contains(s));
        if ev.method == "Target.attachedToTarget" && attach_parent_is_ours {
            if let Some(child) = ev.params.get("sessionId").and_then(|v| v.as_str()) {
                child_sessions.insert(child.to_string());
                let child = child.to_string();
                let child_target = ev
                    .params
                    .get("targetInfo")
                    .and_then(|t| t.get("targetId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                in_flight.push(
                    async move {
                        // The child is attached paused, so its FIRST request —
                        // the one that matters, an out-of-process iframe's own
                        // navigation — cannot outrun this. It is resumed only
                        // once interception is on; if interception could not be
                        // turned on, the target is closed instead of resumed,
                        // because resuming it would run an unvalidated frame and
                        // leaving it paused would stall the render.
                        let enabled = conn
                            .send_recv(
                                "Fetch.enable",
                                serde_json::json!({ "patterns": [{ "urlPattern": "*" }] }),
                                Some(&child),
                                cmd_timeout,
                            )
                            .await
                            .is_ok();
                        if !enabled {
                            crw_core::metrics::metrics()
                                .chrome_blocked_requests_total
                                .with_label_values(&["child_unguarded"])
                                .inc();
                            tracing::warn!(
                                target_id = %child_target,
                                "could not intercept a child target; closing it"
                            );
                            let closed = !child_target.is_empty()
                                && conn
                                    .send_recv(
                                        "Target.closeTarget",
                                        serde_json::json!({ "targetId": child_target }),
                                        None,
                                        cmd_timeout,
                                    )
                                    .await
                                    .is_ok();
                            if !closed {
                                // Deliberately still not resumed: that would run
                                // an unchecked target. The child stays frozen,
                                // which costs this frame and at worst burns the
                                // nav budget. Counted so the trade-off is visible
                                // rather than silent.
                                crw_core::metrics::metrics()
                                    .chrome_blocked_requests_total
                                    .with_label_values(&["child_stuck"])
                                    .inc();
                            }
                            return;
                        }
                        // Auto-attach does not cascade, so ask the child to
                        // attach ITS children too; otherwise an iframe nested
                        // inside an out-of-process iframe never surfaces.
                        let _ = conn
                            .send_recv(
                                "Target.setAutoAttach",
                                serde_json::json!({
                                    "autoAttach": true,
                                    "waitForDebuggerOnStart": true,
                                    "flatten": true,
                                }),
                                Some(&child),
                                cmd_timeout,
                            )
                            .await;
                        let _ = conn
                            .send_recv(
                                "Runtime.runIfWaitingForDebugger",
                                serde_json::json!({}),
                                Some(&child),
                                cmd_timeout,
                            )
                            .await;
                    }
                    .boxed(),
                );
            }
            continue;
        }
        if ev.method != "Fetch.requestPaused" {
            continue;
        }
        let event_session = ev.session_id.clone().unwrap_or_default();
        if event_session != session_id && !child_sessions.contains(&event_session) {
            continue;
        }
        let request_id = ev
            .params
            .get("requestId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if request_id.is_empty() {
            continue;
        }
        let resource_type = ev
            .params
            .get("resourceType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let req_url = ev
            .params
            .get("request")
            .and_then(|r| r.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Every paused request is recorded before a handler takes it, and
        // removed once answered. Teardown fails whatever is left: dropping a
        // handler mid-check would otherwise leave the request paused, and
        // `Fetch.disable` auto-continues paused requests, turning an unfinished
        // check into an allow.
        if let Ok(mut o) = outstanding.lock() {
            o.insert((event_session.clone(), request_id.clone()));
        }

        // Blocklist first: a blocked request never leaves the browser, so there
        // is no destination to validate and no lookup to pay for.
        let blocked_by_list = blocklist
            .and_then(|list| list.should_block(resource_type, &req_url))
            .map(|reason| match reason {
                BlockReason::ResourceType => "resource_type",
                BlockReason::Host => "host",
            });

        in_flight.push(
            async move {
                let block_label = match blocked_by_list {
                    Some(label) => Some(label),
                    None => outbound_block_label(&req_url, ctx).await,
                };
                let (method, params) = match block_label {
                    Some(label) => {
                        crw_core::metrics::metrics()
                            .chrome_blocked_requests_total
                            .with_label_values(&[label])
                            .inc();
                        (
                            "Fetch.failRequest",
                            serde_json::json!({
                                "requestId": request_id,
                                "errorReason": "BlockedByClient",
                            }),
                        )
                    }
                    None => (
                        "Fetch.continueRequest",
                        serde_json::json!({ "requestId": request_id }),
                    ),
                };
                // Answer on the session the event arrived on, which for a child
                // target is not ours.
                // Forget it only once the browser actually took the answer. A
                // `failRequest` that timed out or hit a closed socket leaves the
                // request paused, and dropping the record here would hand it to
                // `Fetch.disable`, which continues paused requests: the same
                // fail-open one layer down. Teardown re-answering an already
                // answered id is harmless, CDP just reports an unknown
                // interception id.
                let answered = conn
                    .send_recv(method, params, Some(&event_session), cmd_timeout)
                    .await
                    .is_ok();
                if answered && let Ok(mut o) = outstanding.lock() {
                    o.remove(&(event_session, request_id));
                }
            }
            .boxed(),
        );
    }
}

/// Capture XHR/fetch JSON responses for fallback extraction.
///
/// Subscribes to CDP events and, for every `Network.responseReceived` whose
/// MIME and status look like API content, calls `Network.getResponseBody` and
/// appends the result. Bounded by `NET_CAPTURE_MAX_BODIES` and
/// `NET_CAPTURE_MAX_TOTAL_BYTES` so a chatty page can't blow memory.
///
/// Never returns under normal operation; the broadcast `Closed` arm exits.
/// Designed to run in `tokio::select!` alongside the main work future.
async fn run_network_capture_pump(
    conn: &CdpConnection,
    mut rx: broadcast::Receiver<CdpEvent>,
    sink: Arc<Mutex<Vec<CapturedNetworkResponse>>>,
    session_id: &str,
) {
    let mut total_bytes = 0usize;
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if ev.method != "Network.responseReceived" {
            continue;
        }
        if ev.session_id.as_deref() != Some(session_id) {
            continue;
        }
        // Skip the main document — already in `html` field.
        let resource_type = ev.params.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(resource_type, "XHR" | "Fetch") {
            continue;
        }
        let response = match ev.params.get("response") {
            Some(v) => v,
            None => continue,
        };
        let status = response
            .get("status")
            .and_then(|s| s.as_f64())
            .map(|s| s as u16)
            .unwrap_or(0);
        if !(200..300).contains(&status) {
            continue;
        }
        let mime = response
            .get("mimeType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !is_capturable_mime(mime) {
            continue;
        }
        // Drop tiny payloads early using Content-Length when available.
        let advertised_len = response
            .get("headers")
            .and_then(|h| h.get("Content-Length").or_else(|| h.get("content-length")))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<usize>().ok());
        if let Some(len) = advertised_len
            && len < NET_CAPTURE_MIN_BODY_SIZE
        {
            continue;
        }
        // Caps before issuing the round-trip.
        {
            let cur = sink.lock().await;
            if cur.len() >= NET_CAPTURE_MAX_BODIES {
                continue;
            }
        }
        if total_bytes >= NET_CAPTURE_MAX_TOTAL_BYTES {
            continue;
        }
        let request_id = ev
            .params
            .get("requestId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if request_id.is_empty() {
            continue;
        }
        let url = response
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let body_resp = match conn
            .send_recv(
                "Network.getResponseBody",
                serde_json::json!({ "requestId": request_id }),
                Some(session_id),
                NET_CAPTURE_GETBODY_TIMEOUT,
            )
            .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let body = body_resp.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let base64 = body_resp
            .get("base64Encoded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if base64 || body.len() < NET_CAPTURE_MIN_BODY_SIZE {
            continue;
        }
        let captured = CapturedNetworkResponse {
            url,
            request_id,
            status,
            mime_type: Some(mime.to_string()),
            body_size_bytes: body.len(),
            body: Some(body.to_string()),
        };
        total_bytes += captured.body_size_bytes;
        sink.lock().await.push(captured);
    }
}

/// Cheap tracker for in-flight network requests. Updated from CDP
/// `Network.requestWillBeSent` / `loadingFinished` / `loadingFailed` events.
/// Used by the SPA poll as an alternate "page settled" signal.
#[derive(Debug)]
struct NetworkActivityTracker {
    /// Net in-flight count. Saturated at 0 in `is_idle` because event ordering
    /// can briefly drive the counter negative (a `loadingFinished` for a
    /// request whose `requestWillBeSent` was missed during pump startup).
    in_flight: AtomicI64,
    /// Wall-clock ms of the last request start/end. Used to gate idle on a
    /// quiet-period — `in_flight == 0` alone fires too early on SPAs that
    /// haven't kicked off their first XHR yet.
    last_change_ms: AtomicI64,
}

impl NetworkActivityTracker {
    fn new() -> Self {
        Self {
            in_flight: AtomicI64::new(0),
            last_change_ms: AtomicI64::new(now_unix_ms()),
        }
    }

    fn record_request_start(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        self.last_change_ms.store(now_unix_ms(), Ordering::Relaxed);
    }

    fn record_request_end(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.last_change_ms.store(now_unix_ms(), Ordering::Relaxed);
    }

    /// Page is "network-idle" once the in-flight count hit zero and stayed
    /// there for at least `quiet_ms`.
    fn is_idle(&self, quiet_ms: i64) -> bool {
        if self.in_flight.load(Ordering::Relaxed) > 0 {
            return false;
        }
        let elapsed = now_unix_ms() - self.last_change_ms.load(Ordering::Relaxed);
        elapsed >= quiet_ms
    }
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Drains CDP events to maintain `tracker`'s in-flight counter. Long-lived
/// like the other pumps; exits when the broadcast closes.
async fn run_network_idle_pump(
    mut rx: broadcast::Receiver<CdpEvent>,
    tracker: Arc<NetworkActivityTracker>,
    session_id: &str,
) {
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if ev.session_id.as_deref() != Some(session_id) {
            continue;
        }
        match ev.method.as_str() {
            "Network.requestWillBeSent" => tracker.record_request_start(),
            "Network.loadingFinished" | "Network.loadingFailed" => tracker.record_request_end(),
            _ => {}
        }
    }
}

/// Whether a MIME type is interesting for content-extraction fallback.
fn is_capturable_mime(mime: &str) -> bool {
    let m = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        m.as_str(),
        "application/json"
            | "application/ld+json"
            | "application/vnd.api+json"
            | "text/json"
            | "text/plain"
    )
}

async fn close_target(conn: &CdpConnection, target_id: &str, renderer: &str) {
    let m = crw_core::metrics::metrics();
    match conn
        .send_recv(
            "Target.closeTarget",
            serde_json::json!({ "targetId": target_id }),
            None,
            TARGET_CLOSE_TIMEOUT,
        )
        .await
    {
        Ok(_) => {
            m.target_lifecycle_total
                .with_label_values(&[renderer, "closed"])
                .inc();
        }
        Err(e) => {
            // closeTarget timed out or returned an error. Page likely
            // still alive in chrome — that's a leak, but we have to
            // move on. Surface as warn so it shows up in operator logs.
            m.target_lifecycle_total
                .with_label_values(&[renderer, "leaked"])
                .inc();
            tracing::warn!(
                renderer,
                target_id,
                error = %e,
                "Target.closeTarget did not complete cleanly; treating as leaked"
            );
        }
    }
}

/// Cancellation-safe reaper for the browser-owned target created during a
/// legacy (pool-off) [`CdpRenderer::fetch_with_ws`].
///
/// The dispatcher wraps that fetch in `tokio::time::timeout` and the fallback
/// ladder can drop it, so a cancellation *mid-`fetch_inner`* would skip the
/// inline `closeTarget` and orphan the target. `Target.createTarget` targets
/// are browser-owned with `disposeOnDetach` unset, so a bare WS close does NOT
/// reap them — the page (and any runaway JS pegging a core) leaks inside the
/// browser. Mirrors the pool path's `PoolGuard::drop`.
///
/// The guard owns the connection: the happy/normal-error path calls
/// [`finish`](WsFetchGuard::finish) to close inline (awaited) and disarm; any
/// cancellation runs [`Drop`], which reaps best-effort on a detached task
/// (Drop cannot await).
struct WsFetchGuard {
    conn: Option<CdpConnection>,
    tid_slot: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    renderer: String,
}

impl WsFetchGuard {
    /// Borrow the connection to run the fetch. Valid until `finish`/drop.
    fn conn(&self) -> &CdpConnection {
        self.conn
            .as_ref()
            .expect("WsFetchGuard::conn after finish/drop")
    }

    /// Detached best-effort reap: `closeTarget` (if a target was created) then
    /// close the WS. Spawned — never awaited inline — so it runs to completion
    /// even if the *caller's* await is cancelled by the outer
    /// `tokio::time::timeout`. An inline `closeTarget().await` could itself be
    /// cancelled mid-flight and re-orphan the target. The task owns `conn`, so
    /// the CDP event loop stays alive to deliver the close before the socket
    /// drops.
    fn spawn_reap(
        conn: CdpConnection,
        tid: Option<String>,
        renderer: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Some(tid) = tid {
                close_target(&conn, &tid, &renderer).await;
            }
            conn.close().await;
        })
    }

    /// Not-cancelled path: spawn the reap and await the handle for backpressure
    /// (keeps the caller's concurrency permit held until cleanup is done). If
    /// *this* await is cancelled by the outer timeout, the spawned reap still
    /// finishes — that's the whole point. Consumes the guard, disarming `Drop`.
    async fn finish(mut self) {
        let conn = self.conn.take().expect("WsFetchGuard::finish called once");
        let tid = self
            .tid_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let renderer = std::mem::take(&mut self.renderer);
        let _ = Self::spawn_reap(conn, tid, renderer).await;
    }
}

impl Drop for WsFetchGuard {
    fn drop(&mut self) {
        // finish() already reaped → conn is None → nothing to do.
        let Some(conn) = self.conn.take() else {
            return;
        };
        // Cancellation before finish(): spawn the same detached reap. Poison-
        // tolerant lock (the repo's `lock_pgids` idiom) so a Drop that runs
        // during unwinding can't double-panic → abort.
        let tid = self
            .tid_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let renderer = std::mem::take(&mut self.renderer);
        if tokio::runtime::Handle::try_current().is_ok() {
            // Detached: discard the JoinHandle, the reap runs to completion on
            // its own. `drop` (not `let _`) so clippy sees the future is
            // intentionally not awaited.
            drop(Self::spawn_reap(conn, tid, renderer));
        }
        // No runtime (non-async teardown) → `conn` drops here, aborting its
        // event loop — the best we can do without a runtime to await on.
    }
}

/// Consume events from `events` until `Page.loadEventFired` (returns the main
/// document status) or a fatal event arrives. Uses `main_document_status`
/// captured from `Network.responseReceived` when available.
async fn wait_for_page_ready(
    mut events: broadcast::Receiver<CdpEvent>,
    session_id: &str,
    timeout: Duration,
) -> CrwResult<u16> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut main_document_status: Option<u16> = None;
    let mut main_frame: Option<String> = None;

    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Err(_) => return Err(CrwError::Timeout(timeout.as_millis() as u64)),
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err(CrwError::RendererError(
                    "CDP event channel closed before load".into(),
                ));
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Ok(ev)) => {
                if ev.session_id.as_deref() != Some(session_id) {
                    continue;
                }
                match ev.method.as_str() {
                    "Network.responseReceived" => {
                        let is_document = ev
                            .params
                            .get("type")
                            .and_then(|v| v.as_str())
                            .is_some_and(|v| v == "Document");
                        // An iframe is a Document too. Without the frame check a
                        // 404 ad/widget frame inside a healthy page stamps the
                        // page 404 — harmless while nothing read the status, but
                        // `ScrapeData::http_error` now fails the page on it.
                        // First Document response wins the main frame; later ones
                        // on that frame are redirects, and last-wins is right.
                        let frame_id = ev.params.get("frameId").and_then(|v| v.as_str());
                        // Adopt only a frame we can actually name. Adopting a
                        // `None` would leave the slot unfilled and let the NEXT
                        // Document response — an iframe — claim it.
                        if is_document && main_frame.is_none() && frame_id.is_some() {
                            main_frame = frame_id.map(str::to_string);
                        }
                        // Until a main frame is known, keep the old behaviour of
                        // taking any Document status rather than reporting none.
                        if is_document
                            && (main_frame.is_none() || frame_id == main_frame.as_deref())
                        {
                            main_document_status = ev
                                .params
                                .get("response")
                                .and_then(|r| r.get("status"))
                                .and_then(|s| s.as_f64())
                                .map(|s| s as u16)
                                .or(main_document_status);
                        }
                    }
                    "Page.loadEventFired" => {
                        return Ok(main_document_status.unwrap_or(200));
                    }
                    "Inspector.targetCrashed" => {
                        return Err(CrwError::RendererError(
                            "Target crashed during render".into(),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
}

#[async_trait]
impl PageFetcher for CdpRenderer {
    async fn fetch(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        wait_for_ms: Option<u64>,
        deadline: crw_core::Deadline,
    ) -> CrwResult<FetchResult> {
        // Overall hard timeout: page_timeout + wait_for + challenge retry budget
        // + content-stability budget (auto-mode only) + overhead. Challenge retries
        // can add up to CHALLENGE_MAX_RETRIES * CHALLENGE_POLL_INTERVAL_MS.
        // When the caller didn't supply `wait_for_ms`, fetch_with_ws uses the
        // SPA selector poll instead of a fixed sleep — size the budget for
        // its worst-case SPA_SELECTOR_MAX_MS rather than the old 2s default.
        let wait_dur = Duration::from_millis(wait_for_ms.unwrap_or(SPA_SELECTOR_MAX_MS));
        let challenge_budget =
            Duration::from_millis(CHALLENGE_POLL_INTERVAL_MS * u64::from(CHALLENGE_MAX_RETRIES));
        let stability_budget = if wait_for_ms.is_none() {
            Duration::from_millis(CONTENT_STABILITY_MAX_MS)
        } else {
            Duration::ZERO
        };
        let internal_timeout =
            self.page_timeout + wait_dur + challenge_budget + stability_budget + FETCH_OVERHEAD;
        // Clamp internal timeout against the caller's remaining budget so the
        // CDP fetch never exceeds the end-to-end deadline. Snapshot remaining
        // once so the diagnostic log and the actual timeout can't disagree
        // across consecutive monotonic reads.
        let remaining = deadline.remaining();
        let overall_timeout = if internal_timeout > remaining {
            tracing::debug!(
                renderer = %self.name,
                internal_ms = internal_timeout.as_millis() as u64,
                remaining_ms = remaining.as_millis() as u64,
                "CDP outer timeout shrunk to fit remaining request deadline. \
                 If the caller supplied an explicit `deadlineMs`, this clamp \
                 is intentional — the request asked for a tighter cap. \
                 Otherwise (issue #35) raise request.deadline_ms_default or \
                 enable request.auto_extend_deadline_for_ladder so per-tier \
                 timeouts get their full configured allowance."
            );
            remaining
        } else {
            internal_timeout
        };
        if overall_timeout.is_zero() {
            // Caller's deadline is already past. Report the budget they were
            // given: the overrun is measured the instant `remaining()` hits
            // zero, so it is always a few milliseconds and reads as a nonsense
            // "Timeout after 1ms".
            return Err(CrwError::Timeout(deadline.requested_ms()));
        }

        let fut = async {
            if let Some(pool) = self.pool.as_ref() {
                self.fetch_with_pool(pool, url, wait_for_ms, deadline).await
            } else {
                self.fetch_with_ws(url, wait_for_ms, deadline).await
            }
        };
        tokio::time::timeout(overall_timeout, fut)
            .await
            .map_err(|_| CrwError::Timeout(overall_timeout.as_millis() as u64))?
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn supports_js(&self) -> bool {
        true
    }

    async fn is_available(&self) -> bool {
        let conn = match self.connect_with_retry().await {
            Ok(conn) => conn,
            Err(_) => return false,
        };
        let check = conn
            .send_recv(
                "Browser.getVersion",
                serde_json::json!({}),
                None,
                Duration::from_secs(5),
            )
            .await;
        conn.close().await;
        check.is_ok()
    }
}

/// Check if HTML looks like a Cloudflare/anti-bot challenge page.
fn is_challenge_page(html: &str) -> bool {
    if html.len() > 50_000 {
        return false;
    }
    let lower = html.to_lowercase();
    lower.contains("just a moment")
        || lower.contains("cf-browser-verification")
        || lower.contains("cf-challenge-running")
        || lower.contains("challenge-platform")
        || (lower.contains("challenge") && lower.contains("cloudflare"))
        || lower.contains("attention required")
}

/// Detect LightPanda/Chrome navigation error pages.
fn detect_navigation_error(html: &str) -> Option<String> {
    if html.len() > 2000 {
        return None;
    }
    let lower = html.to_lowercase();
    if lower.contains("navigation failed") || lower.contains("navigationerror") {
        if let Some(start) = lower.find("reason:") {
            let after = &lower[start + 7..];
            let reason = after
                .split(&['<', '\n'][..])
                .next()
                .unwrap_or("unknown")
                .trim();
            return Some(reason.to_string());
        }
        return Some("unknown".to_string());
    }
    None
}

impl CdpRenderer {
    /// Pool-backed fetch path. Acquires a checked-out browser context from
    /// the pool, runs `fetch_inner` with the slot's ctx_id + a recorder that
    /// writes the new target_id into the slot, then `release()`s the slot
    /// (which owns `closeTarget` + dispose + recreate-ctx).
    async fn fetch_with_pool(
        &self,
        pool: &Arc<crate::browser_pool::BrowserContextPool<CdpConnection>>,
        url: &str,
        wait_for_ms: Option<u64>,
        deadline: crw_core::Deadline,
    ) -> CrwResult<FetchResult> {
        let start = Instant::now();
        let handshake_t0 = Instant::now();
        let acquire_t0 = Instant::now();
        let guard = pool.acquire().await?;
        let acquire_elapsed = acquire_t0.elapsed();
        crw_core::metrics::metrics()
            .chrome_pool_acquire_seconds
            .observe(acquire_elapsed.as_secs_f64());
        // Best-effort acquire-source label: we currently don't surface from
        // `acquire()` whether it hit idle or created new — record under a
        // generic bucket. Plumbing the precise label is a follow-up.
        crw_core::metrics::metrics()
            .chrome_pool_acquires_total
            .with_label_values(&["hit_idle"])
            .inc();

        // Recorder writes the new target_id into the slot synchronously
        // (inside fetch_inner, immediately after createTarget Ok).
        let guard_for_rec = &guard;
        let recorder = |tid: &str| guard_for_rec.record_target(tid.to_string());

        let ctx_id = guard.ctx_id.clone();
        let res = self
            .fetch_inner(
                &guard.conn,
                Some(&ctx_id),
                &recorder,
                url,
                wait_for_ms,
                deadline,
            )
            .await;

        // Record total handshake-overhead for this request (acquire + create
        // target + attach happen inside fetch_inner). B2 gate metric.
        crw_core::metrics::metrics()
            .chrome_request_handshake_seconds
            .with_label_values(&["on", "hit_idle"])
            .observe(handshake_t0.elapsed().as_secs_f64());

        // Always release — swallow recycle error per plan's error-precedence
        // policy (fetch success/failure is what the caller cares about).
        if let Err(e) = guard.release().await {
            tracing::warn!(error = %e, "pool: release returned error (slot recycled as Dead)");
        }

        let (html, status_code, truncated, final_href, captured_responses, _tid) = res?;

        if html.is_empty() {
            return Err(CrwError::RendererError(
                "Empty HTML from CDP renderer".into(),
            ));
        }
        if let Some(reason) = detect_navigation_error(&html) {
            return Err(CrwError::RendererError(format!(
                "Navigation failed: {reason}"
            )));
        }

        let final_url = final_href.filter(|h| h != url);
        Ok(FetchResult {
            url: url.to_string(),
            final_url,
            status_code,
            html,
            content_type: None,
            raw_bytes: None,
            rendered_with: Some(self.name.clone()),
            elapsed_ms: start.elapsed().as_millis() as u64,
            warning: if truncated {
                Some(self.budget_truncated_warning())
            } else {
                None
            },
            render_decision: None,
            credit_cost: 0,
            warnings: if truncated {
                vec![self.budget_truncated_warning()]
            } else {
                Vec::new()
            },
            truncated,
            deadline_exceeded: deadline.remaining().is_zero(),
            captured_responses,
        })
    }

    /// Name the tier that actually ran out of budget. This struct drives every
    /// CDP-speaking renderer, so a hardcoded `chrome_budget_truncated` blamed
    /// Chrome for LightPanda's much smaller budget.
    fn budget_truncated_warning(&self) -> String {
        format!("{}_budget_truncated", self.name)
    }

    /// Inner fetch with WebSocket lifecycle management.
    async fn fetch_with_ws(
        &self,
        url: &str,
        wait_for_ms: Option<u64>,
        deadline: crw_core::Deadline,
    ) -> CrwResult<FetchResult> {
        let start = Instant::now();
        let handshake_t0 = Instant::now();

        // Limit concurrent WebSocket connections to pool_size.
        let _permit = self
            .conn_semaphore
            .acquire()
            .await
            .map_err(|_| CrwError::RendererError("Connection pool closed".into()))?;

        let conn = self.connect_with_retry().await?;

        // Legacy path: `tid_slot` is the SOLE authoritative source for target
        // close on both Ok and Err branches. fetch_inner no longer closes
        // targets itself — we own that here. `Arc<Mutex<...>>` (not `Cell`)
        // so the recorder closure satisfies `Send + Sync` across the await.
        let tid_slot: std::sync::Arc<std::sync::Mutex<Option<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let tid_slot_rec = tid_slot.clone();
        let recorder = move |tid: &str| {
            *tid_slot_rec.lock().unwrap() = Some(tid.to_string());
        };

        // Cancellation-safe cleanup: the dispatcher wraps this whole future in
        // `tokio::time::timeout`, so a mid-`fetch_inner` timeout must not skip
        // the target close (that orphans a browser-owned target → leaked page →
        // 100% CPU spin). The guard owns `conn`; `finish()` closes inline on the
        // not-cancelled path, its `Drop` reaps on cancellation. See WsFetchGuard.
        let guard = WsFetchGuard {
            conn: Some(conn),
            tid_slot: tid_slot.clone(),
            renderer: self.name.clone(),
        };
        let result = self
            .fetch_inner(guard.conn(), None, &recorder, url, wait_for_ms, deadline)
            .await;

        // B2 gate metric: pre-navigation overhead (connect + createTarget +
        // attach). Pool=off arm; pool=on arm is recorded in fetch_with_pool.
        crw_core::metrics::metrics()
            .chrome_request_handshake_seconds
            .with_label_values(&["off", "n/a"])
            .observe(handshake_t0.elapsed().as_secs_f64());

        // Not cancelled: close target + WS inline (awaited), disarm the reaper.
        guard.finish().await;

        let (html, status_code, truncated, final_href, captured_responses, _tid_ignored) = result?;

        if html.is_empty() {
            return Err(CrwError::RendererError(
                "Empty HTML from CDP renderer".into(),
            ));
        }

        if let Some(reason) = detect_navigation_error(&html) {
            return Err(CrwError::RendererError(format!(
                "Navigation failed: {reason}"
            )));
        }

        let final_url = final_href.filter(|h| h != url);

        Ok(FetchResult {
            url: url.to_string(),
            final_url,
            status_code,
            html,
            content_type: None,
            raw_bytes: None,
            rendered_with: Some(self.name.clone()),
            elapsed_ms: start.elapsed().as_millis() as u64,
            warning: if truncated {
                Some(self.budget_truncated_warning())
            } else {
                None
            },
            render_decision: None,
            credit_cost: 0,
            warnings: if truncated {
                vec![self.budget_truncated_warning()]
            } else {
                Vec::new()
            },
            truncated,
            deadline_exceeded: deadline.remaining().is_zero(),
            captured_responses,
        })
    }

    /// Evaluate `window.location.href` to capture the URL after redirects.
    /// Returns `None` on any failure (caller treats this as "no redirect known").
    async fn eval_href(
        conn: &CdpConnection,
        session_id: &str,
        timeout: Duration,
    ) -> Option<String> {
        let eval_result = conn
            .send_recv(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "window.location.href",
                    "returnByValue": true
                }),
                Some(session_id),
                timeout,
            )
            .await
            .ok()?;
        eval_result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Scroll the page viewport-by-viewport until `document.body.scrollHeight`
    /// stops growing or `AUTO_SCROLL_MAX_STEPS` is reached. Triggers lazy-loaded
    /// images, infinite-scroll feeds, and below-the-fold hydration.
    ///
    /// Best-effort: failures swallowed (debug log) so a flaky evaluate doesn't
    /// abort the whole render.
    async fn auto_scroll(conn: &CdpConnection, session_id: &str, timeout: Duration) {
        let script = format!(
            r#"
            (async () => {{
                const sleep = (ms) => new Promise(r => setTimeout(r, ms));
                const max_steps = {max_steps};
                const step_delay = {delay};
                let last_h = 0;
                let stable = 0;
                let steps = 0;
                for (let i = 0; i < max_steps; i++) {{
                    steps++;
                    window.scrollBy(0, window.innerHeight || 800);
                    await sleep(step_delay);
                    const h = document.body ? document.body.scrollHeight : 0;
                    if (h <= last_h) {{ stable++; if (stable >= 2) break; }} else {{ stable = 0; }}
                    last_h = h;
                }}
                window.scrollTo(0, 0);
                return {{ steps, final_height: last_h }};
            }})()
            "#,
            max_steps = AUTO_SCROLL_MAX_STEPS,
            delay = AUTO_SCROLL_STEP_DELAY_MS,
        );
        let result = conn
            .send_recv(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": script,
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
                Some(session_id),
                timeout,
            )
            .await;
        match result {
            Ok(v) => tracing::debug!(?v, "auto_scroll completed"),
            Err(e) => tracing::debug!(error = %e, "auto_scroll failed (non-fatal)"),
        }
    }

    /// Click `[aria-expanded=false]` toggles and "load more" / "show full"
    /// buttons that hide content behind a click. Bounded by
    /// `AUTO_CLICK_MAX_CLICKS` and `AUTO_CLICK_BUDGET_MS`. Skips submit /
    /// link / external-nav buttons by checking element types and `<a>` tags.
    /// Best-effort — failures swallowed so a flaky evaluate doesn't abort
    /// the whole render.
    async fn auto_click_reveal(conn: &CdpConnection, session_id: &str, timeout: Duration) {
        let script = format!(
            r#"
            (async () => {{
                const sleep = (ms) => new Promise(r => setTimeout(r, ms));
                const max_clicks = {max_clicks};
                const delay = {delay};
                const REVEAL_RE = /^(load|show|read|view|see|expand)\s*(more|full|all|details?)?\b|^more\b|^expand\b/i;
                const candidates = new Set();
                // aria-expanded toggles
                document.querySelectorAll('[aria-expanded="false"]').forEach(el => {{
                    if (el.tagName !== 'A') candidates.add(el);
                }});
                // text-matching buttons / clickable divs
                document.querySelectorAll('button, [role="button"], summary').forEach(el => {{
                    const text = (el.innerText || el.textContent || '').trim();
                    if (text && text.length < 40 && REVEAL_RE.test(text)) candidates.add(el);
                }});
                let clicks = 0;
                for (const el of candidates) {{
                    if (clicks >= max_clicks) break;
                    if (!el.isConnected) continue;
                    // Skip elements outside the viewport range — we don't
                    // want to scroll-to-element on nav drawers.
                    const rect = el.getBoundingClientRect();
                    if (rect.bottom < -2000 || rect.top > 20000) continue;
                    try {{ el.click(); clicks++; await sleep(delay); }} catch (e) {{ /* ignore */ }}
                }}
                return {{ clicks }};
            }})()
            "#,
            max_clicks = AUTO_CLICK_MAX_CLICKS,
            delay = AUTO_CLICK_DELAY_MS,
        );
        let result = conn
            .send_recv(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": script,
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
                Some(session_id),
                timeout,
            )
            .await;
        match result {
            Ok(v) => tracing::debug!(?v, "auto_click_reveal completed"),
            Err(e) => tracing::debug!(error = %e, "auto_click_reveal failed (non-fatal)"),
        }
    }

    /// Best-effort consent banner dismissal. Errors are swallowed — a missing
    /// banner, sandboxed iframe, or unsupported `__tcfapi` shouldn't fail the
    /// fetch. The script returns the click count for telemetry but we don't
    /// surface it on the FetchResult yet (would need a new field).
    async fn dismiss_consent(conn: &CdpConnection, session_id: &str) {
        let res = conn
            .send_recv(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": CMP_DISMISS_JS,
                    "returnByValue": true,
                }),
                Some(session_id),
                Duration::from_secs(2),
            )
            .await;
        match res {
            Ok(v) => {
                let clicks = v
                    .get("result")
                    .and_then(|r| r.get("value"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                if clicks > 0 {
                    tracing::debug!(clicks, "consent banner dismissed");
                }
            }
            Err(e) => tracing::debug!("CMP dismiss eval failed: {e}"),
        }
    }

    async fn eval_html(
        conn: &CdpConnection,
        session_id: &str,
        timeout: Duration,
    ) -> CrwResult<String> {
        // Tier 0 metric M3: HTML snapshot round-trip. Observed on every call
        // (post-navigate, SPA poll, scroll re-snapshot) so we can see how the
        // snapshot mix behaves under different page types.
        let snap_t0 = Instant::now();
        let eval_result = conn
            .send_recv(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": HTML_SNAPSHOT_JS,
                    "returnByValue": true
                }),
                Some(session_id),
                timeout,
            )
            .await?;
        crw_core::metrics::metrics()
            .chrome_snapshot_seconds
            .observe(snap_t0.elapsed().as_secs_f64());

        Ok(eval_result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// Poll for SPA readiness up to [`SPA_SELECTOR_MAX_MS`]. Three exit
    /// conditions, any of which counts as "ready":
    ///   * selector mounted AND body innerText ≥ [`SPA_BODY_TEXT_MIN_CHARS`]
    ///     — covers static + already-hydrated pages (one tick, fast path)
    ///   * selector mounted AND network has been idle for ≥
    ///     [`NETWORK_IDLE_QUIET_MS`] — covers SPAs whose XHR fetches finished
    ///     but body text is still under the threshold (the page is done; the
    ///     content just isn't bulky)
    ///   * budget elapses — caller proceeds with whatever's there
    ///
    /// Single eval per tick returns the body text length (or -1 when the
    /// selector is missing). Healthy pages with text already present clear
    /// on the first poll. The network-idle gate requires the selector to be
    /// mounted first so we don't exit on the pre-navigate-fetch idle window.
    async fn wait_for_spa_selector(
        conn: &CdpConnection,
        session_id: &str,
        timeout: Duration,
        net: &NetworkActivityTracker,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_millis(SPA_SELECTOR_MAX_MS);
        let expr = format!(
            r#"(() => {{
                if (!document.querySelector({sel:?})) return -1;
                const t = (document.body && document.body.innerText) || "";
                return t.trim().length;
            }})()"#,
            sel = SPA_CONTENT_SELECTORS
        );
        while Instant::now() < deadline {
            match conn
                .send_recv(
                    "Runtime.evaluate",
                    serde_json::json!({ "expression": expr, "returnByValue": true }),
                    Some(session_id),
                    timeout,
                )
                .await
            {
                Ok(v) => {
                    let len = v
                        .get("result")
                        .and_then(|r| r.get("value"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(-1);
                    let selector_mounted = len >= 0;
                    if is_spa_text_ready(len) {
                        return true;
                    }
                    if selector_mounted && net.is_idle(NETWORK_IDLE_QUIET_MS) {
                        tracing::debug!(
                            text_len = len,
                            "SPA poll exiting on network-idle (selector mounted, text below threshold)"
                        );
                        return true;
                    }
                }
                Err(e) => {
                    tracing::debug!("SPA selector poll eval failed: {e}");
                    return false;
                }
            }
            tokio::time::sleep(Duration::from_millis(SPA_SELECTOR_TICK_MS)).await;
        }
        false
    }

    /// Poll `document.documentElement.outerHTML` at a fixed interval until the
    /// rendered HTML stabilises and no longer looks like a loading placeholder,
    /// or until the stability budget is exhausted.
    async fn poll_until_content_stable(
        conn: &CdpConnection,
        session_id: &str,
        timeout: Duration,
    ) -> CrwResult<String> {
        let deadline = Instant::now() + Duration::from_millis(CONTENT_STABILITY_MAX_MS);
        let mut prev_len: u64 = 0;
        let mut stable_ticks: u32 = 0;
        let mut last_html = String::new();

        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(CONTENT_STABILITY_TICK_MS)).await;
            let html = Self::eval_html(conn, session_id, timeout).await?;
            let len = html.len() as u64;
            let placeholder_gone = !crate::detector::looks_like_loading_placeholder(&html);
            if is_content_stable(prev_len, len, placeholder_gone) {
                stable_ticks += 1;
                if stable_ticks >= 2 {
                    return Ok(html);
                }
            } else {
                stable_ticks = 0;
            }
            prev_len = len;
            last_html = html;
        }
        Ok(last_html)
    }

    async fn fetch_inner(
        &self,
        conn: &CdpConnection,
        browser_context_id: Option<&str>,
        target_recorder: &(dyn Fn(&str) + Send + Sync),
        url: &str,
        wait_for_ms: Option<u64>,
        deadline: crw_core::Deadline,
    ) -> CrwResult<(
        String,
        u16,
        bool,
        Option<String>,
        Vec<CapturedNetworkResponse>,
        String,
    )> {
        // 1. Create a blank target so navigation events can be observed reliably.
        // When `browser_context_id` is Some, the target is bound to that
        // context — load-bearing for pool isolation (cookies/storage do not
        // leak across contexts).
        let create_t0 = Instant::now();
        let mut create_params = serde_json::json!({ "url": "about:blank" });
        if let Some(ctx) = browser_context_id {
            create_params["browserContextId"] = serde_json::Value::String(ctx.to_string());
        }
        let create_result = conn
            .send_recv(
                "Target.createTarget",
                create_params,
                None,
                self.page_timeout,
            )
            .await?;
        crw_core::metrics::metrics()
            .chrome_target_create_seconds
            .observe(create_t0.elapsed().as_secs_f64());

        let target_id = create_result
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CrwError::RendererError(format!("No targetId: {create_result}")))?
            .to_string();
        // CRITICAL — synchronous handoff to caller BEFORE any subsequent
        // `.await`. This is the only sync point that closes the cancellation
        // window between createTarget returning Ok and the next await. The
        // pooled caller writes the id into the slot's `CheckedOut.target_id`
        // here; the legacy caller writes into a stack-local `Cell`. Either way
        // the caller owns `closeTarget` from this point on — fetch_inner does
        // NOT call closeTarget on any branch.
        target_recorder(&target_id);
        crw_core::metrics::metrics()
            .target_lifecycle_total
            .with_label_values(&[&self.name, "created"])
            .inc();

        // 2. Attach to target
        let attach_result = conn
            .send_recv(
                "Target.attachToTarget",
                serde_json::json!({ "targetId": &target_id, "flatten": true }),
                None,
                self.page_timeout,
            )
            .await?;

        let session_id = attach_result
            .get("sessionId")
            .and_then(|value| value.as_str())
            .ok_or_else(|| CrwError::RendererError("CDP attach did not return sessionId".into()))?
            .to_string();

        for method in ["Page.enable", "Network.enable", "Runtime.enable"] {
            conn.send_recv(
                method,
                serde_json::json!({}),
                Some(&session_id),
                self.page_timeout,
            )
            .await?;
        }

        // Inject stealth scripts before navigation so they run on every new document.
        conn.send_recv(
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({ "source": STEALTH_JS }),
            Some(&session_id),
            self.page_timeout,
        )
        .await?;

        // Present a modern UA on the CDP path too (the HTTP fetcher already does,
        // but renderers otherwise send the browser's own — often stale — UA, which
        // trips "your browser is outdated" gates). Session-scoped (so pooled
        // contexts don't leak it) and best-effort: a tier that rejects the method
        // must NOT abort an otherwise-fine render — hence `.ok()`, not `?`.
        // lightpanda rejects "Mozilla" UAs (→ `lightpanda_safe_ua`); it routes
        // Network.* → Emulation.* internally, so the method name is fine. Skip if empty.
        if !self.user_agent.is_empty() {
            let ua = if self.name == "lightpanda" {
                lightpanda_safe_ua(&self.user_agent)
            } else {
                &self.user_agent
            };
            conn.send_recv(
                "Network.setUserAgentOverride",
                serde_json::json!({ "userAgent": ua }),
                Some(&session_id),
                self.page_timeout,
            )
            .await
            .ok();
        }

        // Subscribe to events BEFORE navigating so we don't miss loadEventFired.
        let events_rx = conn.subscribe();

        // Compose per-call DataImpulse credentials for the chrome_proxy tier.
        // Country priority: per-request (task-local) → renderer default → none.
        // Two-letter lowercase ASCII guard mirrors `RendererConfig::effective_proxy_credentials`.
        let effective_creds: Option<(String, String)> =
            self.proxy_auth_base.as_ref().map(|(base_user, base_pass)| {
                let req_country = crate::REQUEST_COUNTRY
                    .try_with(|c| c.clone())
                    .ok()
                    .flatten();
                let cc = req_country
                    .as_deref()
                    .or(self.default_country.as_deref())
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()));
                match cc {
                    Some(cc) => (format!("{base_user}__cr.{cc}"), base_pass.clone()),
                    None => (base_user.clone(), base_pass.clone()),
                }
            });
        let auth_active = effective_creds.is_some();

        // Enable request interception. Must be done before `Page.navigate`
        // because `Fetch.enable` pauses the document request too — the pump must
        // already be consuming `Fetch.requestPaused` by then.
        //
        // Unconditional: the pump is what validates every destination the
        // browser reaches on its own (redirects, JS navigation, iframes, XHR),
        // which the route-layer check cannot see. `intercept_active` now decides
        // only whether the ad/resource blocklist runs alongside that check.
        //
        // Patterns are ALWAYS `[*]`. Chrome rejects an empty `patterns` array
        // together with `handleAuthRequests: true` (`-32602 Can't specify empty
        // patterns with handleAuth set`), which silently broke every proxy that
        // needs auth — e.g. DataImpulse — on the Chrome path. With `[*]` the
        // browser pauses every request, so the pump MUST answer them all or the
        // page hangs.
        //
        // A failure here propagates: a tier whose destinations we cannot check
        // must not be used, and the ladder falls through to the next one.
        let intercept_active = self.intercept_active_for(url);
        {
            let mut params = serde_json::Map::new();
            params.insert(
                "patterns".into(),
                serde_json::json!([{ "urlPattern": "*" }]),
            );
            if auth_active {
                params.insert("handleAuthRequests".into(), serde_json::json!(true));
            }
            conn.send_recv(
                "Fetch.enable",
                serde_json::Value::Object(params),
                Some(&session_id),
                self.page_timeout,
            )
            .await?;
        }

        // Auto-attach child targets (out-of-process iframes, workers) so the
        // pump can enable interception on them as they appear. Without this
        // their requests never surface on our session and escape the
        // destination check entirely. `waitForDebuggerOnStart: true` because the
        // request that matters is the child's own first navigation, which would
        // otherwise outrun `Fetch.enable`; the pump resumes every child it sees,
        // and a child it cannot intercept is closed rather than resumed. Best-effort — a browser without
        // the command keeps the previous behaviour rather than failing the
        // render.
        let _ = conn
            .send_recv(
                "Target.setAutoAttach",
                serde_json::json!({
                    "autoAttach": true,
                    "waitForDebuggerOnStart": true,
                    "flatten": true,
                }),
                Some(&session_id),
                self.page_timeout,
            )
            .await;
        // Not repeated at browser scope. Service workers and shared workers are
        // not children of the page target, so this does not reach them, and
        // their `fetch` is the one remaining path that can carry a response back
        // into the page. Browser-scope auto-attach delivers those events with no
        // `sessionId`, so the pump cannot tell which render they belong to
        // without tracking browser contexts. Left as a known gap; on LightPanda
        // `--block-private-networks` covers it, on chrome it does not.

        // Network-idle tracker fed by a sibling pump (spawned below in the
        // select!). Created before the `work` future because `work` borrows
        // it for the SPA poll's idle-exit gate.
        let net_tracker = Arc::new(NetworkActivityTracker::new());

        // The work future drives navigate → wait_for_load → post-navigate work.
        // It races against the interception pump via `tokio::select!`. The
        // pump never returns; when work completes, the pump future is dropped.
        let nav_budget = self.nav_budget.min(deadline.remaining());
        let work = async {
            // Tier 0 metric M2: time Page.navigate send → loadEventFired.
            let nav_t0 = Instant::now();
            let navigate_result = conn
                .send_recv(
                    "Page.navigate",
                    serde_json::json!({ "url": url }),
                    Some(&session_id),
                    self.page_timeout,
                )
                .await?;
            if let Some(error_text) = navigate_result
                .get("errorText")
                .and_then(|value| value.as_str())
            {
                return Err(CrwError::RendererError(format!(
                    "Navigation failed: {error_text}"
                )));
            }
            let status_code =
                wait_for_page_ready(events_rx, &session_id, self.page_timeout).await?;
            crw_core::metrics::metrics()
                .chrome_navigate_seconds
                .observe(nav_t0.elapsed().as_secs_f64());
            // Post-navigate phase runs inside a budget race. On budget hit
            // we attempt a partial-DOM snapshot and return `truncated = true`;
            // `single.rs` decides success on md length.
            let phase = self.post_navigate_phase(conn, &session_id, url, wait_for_ms, &net_tracker);
            let (html, truncated) = match tokio::time::timeout(nav_budget, phase).await {
                Ok(Ok(html)) => (html, false),
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    // Name the tier that actually ran out, for the same reason
                    // `budget_truncated_warning()` does: this struct drives every
                    // CDP-speaking renderer, so a hardcoded "chrome" blamed Chrome
                    // for LightPanda's much smaller budget and sent anyone reading
                    // the logs to the wrong tier.
                    tracing::info!(
                        url,
                        renderer = %self.name,
                        budget_ms = nav_budget.as_millis() as u64,
                        "nav budget hit; attempting partial snapshot"
                    );
                    let _ = conn
                        .send_recv(
                            "Page.stopLoading",
                            serde_json::json!({}),
                            Some(&session_id),
                            Duration::from_secs(1),
                        )
                        .await;
                    let html = Self::eval_html(conn, &session_id, Duration::from_secs(2))
                        .await
                        .unwrap_or_default();
                    crw_core::metrics::metrics()
                        .chrome_budget_truncated_total
                        .with_label_values(&[if html.is_empty() { "empty" } else { "ok" }])
                        .inc();
                    (html, true)
                }
            };
            Ok::<_, CrwError>((html, status_code, truncated))
        };

        // Always-on XHR/fetch capture. Cheap when no JSON XHRs fire (events
        // skipped by mime/type filter). Bounded by NET_CAPTURE_MAX_BODIES and
        // NET_CAPTURE_MAX_TOTAL_BYTES so a chatty page can't OOM us.
        let captured: Arc<Mutex<Vec<CapturedNetworkResponse>>> = Arc::new(Mutex::new(Vec::new()));
        let cap_pump =
            run_network_capture_pump(conn, conn.subscribe(), captured.clone(), &session_id);

        // Network-idle pump (tracker constructed earlier). Fed by `Network.*`
        // events; SPA poll consults the tracker for an early exit when XHR
        // traffic settles before body innerText hits the threshold.
        let idle_pump = run_network_idle_pump(conn.subscribe(), net_tracker.clone(), &session_id);

        // The intercept pump always runs; only the blocklist half is optional.
        let pump_blocklist = intercept_active.then_some(&self.blocklist);
        let outbound_ctx = OutboundCtx {
            memo: Arc::new(StdMutex::new(HashMap::new())),
            render_limit: tokio::sync::Semaphore::new(PER_RENDER_RESOLVE_LIMIT),
            // A verdict that arrives after the request is already lost helps
            // nobody, so the check is bounded by the smallest thing that can end
            // it: the caller's remaining deadline, the tier's outer timeout, and
            // the post-navigate budget. Using `page_timeout` alone would have
            // given chrome 30s, well past the point the render is abandoned.
            // Accepted cost: on a tier with a small ceiling (LightPanda is
            // 2500ms) a cold lookup that queues can expire and drop that one
            // subresource, counted as `outbound_unresolved`.
            budget: self
                .nav_budget
                .min(self.page_timeout)
                .min(deadline.remaining()),
            doc_host: url::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default(),
            unresolved: std::sync::atomic::AtomicBool::new(false),
        };
        let outstanding: Outstanding = Arc::new(StdMutex::new(std::collections::HashSet::new()));
        let intercept_pump = run_intercept_pump(
            conn,
            conn.subscribe(),
            pump_blocklist,
            &session_id,
            &outbound_ctx,
            &outstanding,
        );
        let outcome = if auth_active {
            let auth_pump =
                run_auth_pump(conn, conn.subscribe(), effective_creds.clone(), &session_id);
            tokio::select! {
                biased;
                res = work => res,
                _ = intercept_pump => Err(CrwError::RendererError(
                    "interception pump exited unexpectedly".into(),
                )),
                _ = auth_pump => Err(CrwError::RendererError(
                    "auth pump exited unexpectedly".into(),
                )),
                _ = cap_pump => Err(CrwError::RendererError(
                    "network capture pump exited unexpectedly".into(),
                )),
                _ = idle_pump => Err(CrwError::RendererError(
                    "network idle pump exited unexpectedly".into(),
                )),
            }
        } else {
            tokio::select! {
                biased;
                res = work => res,
                _ = intercept_pump => Err(CrwError::RendererError(
                    "interception pump exited unexpectedly".into(),
                )),
                _ = cap_pump => Err(CrwError::RendererError(
                    "network capture pump exited unexpectedly".into(),
                )),
                _ = idle_pump => Err(CrwError::RendererError(
                    "network idle pump exited unexpectedly".into(),
                )),
            }
        };

        // Any request whose check was still running when the pump was dropped is
        // still paused. Failing them explicitly releases the browser's own state
        // now rather than at target close, and makes the denial observable; the
        // target close behind us is the backstop, not the mechanism.
        let leftover: Vec<(String, String)> = outstanding
            .lock()
            .map(|o| o.iter().cloned().collect())
            .unwrap_or_default();
        if !leftover.is_empty() {
            use futures::stream::{FuturesUnordered, StreamExt};
            // Concurrent, under one overall cap. The pump has no in-flight limit
            // by design, so this set is unbounded, and failing them serially
            // would run past the caller's remaining deadline and let the outer
            // timeout fire in the middle of cleanup.
            let mut pending: FuturesUnordered<_> = leftover
                .into_iter()
                .map(|(sess, req_id)| async move {
                    let _ = conn
                        .send_recv(
                            "Fetch.failRequest",
                            serde_json::json!({
                                "requestId": req_id,
                                "errorReason": "BlockedByClient",
                            }),
                            Some(&sess),
                            Duration::from_secs(1),
                        )
                        .await;
                })
                .collect();
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                while pending.next().await.is_some() {}
            })
            .await;
        }

        // Deliberately no `Fetch.disable`. It auto-continues still-paused
        // requests, and the ones it would release are exactly the ones the guard
        // never got to judge: a `Fetch.requestPaused` still sitting in the
        // broadcast ring when the pump was dropped, or one lost to a `Lagged`
        // drop, is in no set we can fail explicitly. Disabling would let those
        // out unchecked, which is the hole this guard exists to close.
        //
        // Nothing leaks by leaving it on. `Fetch.enable` was sent against this
        // session, every render creates its own target and therefore its own
        // session, and the caller always closes that target: the legacy path
        // right after `fetch_inner` returns, the pooled path in `release()`,
        // which skips the close only in the cases where no target was ever
        // created. Anything still paused dies with the target, which is the
        // fail-closed outcome.
        // A denial the guard could not decide is our failure, not the origin's.
        // Both tiers resolve through the same resolver, so leaving this as a
        // navigation failure lets a DNS brown-out be reported as an unreachable
        // target: refunded, and invisible to the 5xx watchdog.
        let outcome = match outcome {
            Err(err)
                if outbound_ctx
                    .unresolved
                    .load(std::sync::atomic::Ordering::Relaxed) =>
            {
                tracing::warn!(url, tier = %self.name, "outbound destination check unavailable");
                Err(CrwError::RendererError(format!(
                    "outbound destination check unavailable ({err})"
                )))
            }
            other => other,
        };

        // Capture final URL after any redirects, before tearing down the target.
        // Best-effort: failures map to None and never propagate.
        let final_href = match outcome.as_ref() {
            Ok(_) => Self::eval_href(conn, &session_id, Duration::from_secs(2)).await,
            Err(_) => None,
        };

        // Target close is the caller's responsibility (pool's release() owns
        // it via the recorded target_id; legacy fetch_with_ws closes after
        // fetch_inner returns via its Cell-captured id).

        let (html, status_code, truncated) = outcome?;

        if html.is_empty() && truncated {
            return Err(CrwError::Timeout(nav_budget.as_millis() as u64));
        }

        if !truncated
            && wait_for_ms.is_none()
            && crate::detector::looks_like_loading_placeholder(&html)
        {
            tracing::debug!(url, "Placeholder still present after stability poll");
        }

        let captured_drained = std::mem::take(&mut *captured.lock().await);
        Ok((
            html,
            status_code,
            truncated,
            final_href,
            captured_drained,
            target_id,
        ))
    }

    /// Post-navigate work: SPA selector wait, eval HTML, placeholder
    /// stability poll, challenge retry. Lives inside a `nav_budget` race;
    /// see `fetch_inner` for the timeout/snapshot fallback path.
    async fn post_navigate_phase(
        &self,
        conn: &CdpConnection,
        session_id: &str,
        url: &str,
        wait_for_ms: Option<u64>,
        net: &NetworkActivityTracker,
    ) -> CrwResult<String> {
        // 2.5. Best-effort consent / CMP dismissal. Cookie banners can both
        // hide content behind an overlay and inflate `body.innerText` past
        // the SPA-readiness threshold prematurely (the banner copy alone
        // can clear 800 chars). Auto-mode only: if the caller pinned a
        // wait, they own the timing.
        if wait_for_ms.is_none() {
            Self::dismiss_consent(conn, session_id).await;
        }

        // 3. Wait for initial JS work. Caller-supplied `wait_for_ms` wins —
        // sleep that long. Otherwise jump straight to the SPA selector poll;
        // the poll exits in ~200ms on static pages where `main`/`article`/etc.
        // are already mounted, and waits up to SPA_SELECTOR_MAX_MS for SPAs
        // that hydrate after `loadEventFired`.
        if let Some(wait) = wait_for_ms {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        } else if !Self::wait_for_spa_selector(conn, session_id, self.page_timeout, net).await {
            tracing::debug!(url, "SPA selector poll exhausted budget");
        }

        // 4. Get rendered HTML.
        let mut html = Self::eval_html(conn, session_id, self.page_timeout).await?;

        // 4b. SPA loading placeholder → poll for content stability.
        if wait_for_ms.is_none() && crate::detector::looks_like_loading_placeholder(&html) {
            tracing::info!(
                url,
                "Loading placeholder detected, polling for content stability"
            );
            match Self::poll_until_content_stable(conn, session_id, self.page_timeout).await {
                Ok(stable) => html = stable,
                Err(e) => tracing::warn!("Content stability polling failed: {e}"),
            }
        }

        // 4c. Auto-mode lazy-load pass: scroll viewport-by-viewport so
        // infinite-scroll feeds, lazy images, and below-the-fold hydration
        // appear in the snapshot. Gated:
        // - skip when caller pinned wait_for_ms (explicit budget)
        // - skip on challenge / placeholder shells (nothing to scroll)
        // - skip when HTML is already large (almost certainly fully-rendered)
        //   unless we see explicit lazy-load markers
        // Stricter gate: only scroll when explicit lazy-load markers exist.
        // Empirically, scrolling healthy pages adds latency without lift, and
        // pushes some heavy renders past the deadline.
        let has_lazy_markers = html.contains("loading=\"lazy\"")
            || html.contains("data-src=")
            || html.contains("infinite-scroll")
            || html.contains("lazy-load");
        if wait_for_ms.is_none()
            && has_lazy_markers
            && html.len() < AUTO_SCROLL_HTML_SIZE_LIMIT
            && !is_challenge_page(&html)
            && !crate::detector::looks_like_loading_placeholder(&html)
        {
            let scroll_timeout = Duration::from_millis(AUTO_SCROLL_BUDGET_MS);
            let scroll_timeout = scroll_timeout.min(self.page_timeout);
            tokio::time::timeout(
                scroll_timeout,
                Self::auto_scroll(conn, session_id, scroll_timeout),
            )
            .await
            .ok();
            // Re-snapshot after scrolling so any lazy-loaded content is captured.
            html = Self::eval_html(conn, session_id, self.page_timeout).await?;
        }

        // 4d. Click-to-reveal pass: expand collapsed accordions / "load more"
        // CTAs that hide article body behind a click. Gated to pages that
        // actually have markers — pure content pages are skipped.
        let has_reveal_markers = html.contains(r#"aria-expanded="false""#)
            || html.contains("load-more")
            || html.contains("show-more")
            || {
                let lower = html.to_ascii_lowercase();
                lower.contains(">load more<")
                    || lower.contains(">show more<")
                    || lower.contains(">read more<")
                    || lower.contains(">show full<")
                    || lower.contains(">view all<")
            };
        if wait_for_ms.is_none()
            && has_reveal_markers
            && html.len() < AUTO_SCROLL_HTML_SIZE_LIMIT
            && !is_challenge_page(&html)
            && !crate::detector::looks_like_loading_placeholder(&html)
        {
            let click_timeout = Duration::from_millis(AUTO_CLICK_BUDGET_MS);
            let click_timeout = click_timeout.min(self.page_timeout);
            tokio::time::timeout(
                click_timeout,
                Self::auto_click_reveal(conn, session_id, click_timeout),
            )
            .await
            .ok();
            html = Self::eval_html(conn, session_id, self.page_timeout).await?;
        }

        // 5. Challenge retry loop for Cloudflare/anti-bot interstitials.
        if is_challenge_page(&html) {
            tracing::info!(url, "Challenge page detected, waiting for auto-resolve");
            for attempt in 1..=CHALLENGE_MAX_RETRIES {
                tokio::time::sleep(Duration::from_millis(CHALLENGE_POLL_INTERVAL_MS)).await;
                html = Self::eval_html(conn, session_id, self.page_timeout).await?;
                if !is_challenge_page(&html) {
                    tracing::info!(url, attempt, "Challenge cleared");
                    break;
                }
                tracing::debug!(url, attempt, "Challenge still active, retrying");
            }
        }

        Ok(html)
    }
}

/// Pure decision: does the current poll tick indicate the rendered page has
/// stabilised? Returns `false` on the first tick (`prev_len == 0`) so that at
/// least two observations are required. `placeholder_gone` must be `true`
/// (the rendered HTML no longer looks like a loading placeholder).
///
/// Size tolerance is 5% of `prev_len` with a 500-byte floor, so noise from
/// small DOM updates (timestamps, counters) does not reset stability.
fn is_content_stable(prev_len: u64, curr_len: u64, placeholder_gone: bool) -> bool {
    if prev_len == 0 || !placeholder_gone {
        return false;
    }
    let tolerance = (prev_len / 20).max(500);
    curr_len.abs_diff(prev_len) <= tolerance
}

/// Pure decision: does the SPA poll tick indicate the page is ready to
/// snapshot? `text_len` is the body innerText length returned from the JS
/// eval, with `-1` signaling "selector not yet mounted". Threshold matches
/// [`SPA_BODY_TEXT_MIN_CHARS`].
fn is_spa_text_ready(text_len: i64) -> bool {
    text_len >= SPA_BODY_TEXT_MIN_CHARS as i64
}

#[cfg(test)]
mod tests {
    use super::{
        CdpRenderer, build_auth_response, is_content_stable, lightpanda_safe_ua,
        outbound_block_label,
    };

    #[test]
    fn budget_truncated_warning_names_the_tier_that_ran_out() {
        // One struct drives every CDP tier, so a hardcoded string reported
        // LightPanda's truncation as Chrome's and sent debuggers to the wrong
        // renderer.
        let lp = CdpRenderer::new("lightpanda", "ws://x/", 1000, 1);
        assert_eq!(lp.budget_truncated_warning(), "lightpanda_budget_truncated");
    }

    #[test]
    fn auth_response_provides_credentials_when_creds_set() {
        let v = build_auth_response("req-1", Some(("abc__cr.us", "pw")));
        assert_eq!(v["requestId"], "req-1");
        assert_eq!(v["authChallengeResponse"]["response"], "ProvideCredentials");
        assert_eq!(v["authChallengeResponse"]["username"], "abc__cr.us");
        assert_eq!(v["authChallengeResponse"]["password"], "pw");
    }

    #[test]
    fn auth_response_cancels_when_no_creds() {
        let v = build_auth_response("req-2", None);
        assert_eq!(v["authChallengeResponse"]["response"], "CancelAuth");
        assert!(v["authChallengeResponse"].get("username").is_none());
        assert!(v["authChallengeResponse"].get("password").is_none());
    }

    #[test]
    fn auth_response_no_password_leak_on_cancel() {
        // Sanity: a CancelAuth payload never carries credentials, even if the
        // caller mistakenly passes some out-of-band data. (Defense-in-depth
        // smoke test — guards against future shape changes.)
        let v = build_auth_response("req-3", None);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("\"password\""));
        assert!(!s.contains("\"username\""));
    }

    #[test]
    fn first_tick_never_stable() {
        assert!(!is_content_stable(0, 0, true));
        assert!(!is_content_stable(0, 10_000, true));
    }

    #[test]
    fn identical_sizes_are_stable_when_placeholder_gone() {
        assert!(is_content_stable(5_000, 5_000, true));
    }

    #[test]
    fn placeholder_still_present_blocks_stability() {
        assert!(!is_content_stable(5_000, 5_000, false));
    }

    #[test]
    fn small_delta_within_tolerance_is_stable() {
        assert!(is_content_stable(10_000, 10_400, true));
    }

    #[test]
    fn large_delta_outside_tolerance_is_unstable() {
        assert!(!is_content_stable(10_000, 12_000, true));
    }

    #[test]
    fn small_page_uses_500_byte_floor() {
        assert!(is_content_stable(100, 450, true));
    }

    #[test]
    fn shrink_past_tolerance_is_unstable() {
        assert!(!is_content_stable(10_000, 5_000, true));
    }

    use super::{SPA_BODY_TEXT_MIN_CHARS, is_spa_text_ready};

    #[test]
    fn spa_not_ready_when_selector_missing() {
        assert!(!is_spa_text_ready(-1));
    }

    #[test]
    fn spa_not_ready_when_text_below_threshold() {
        assert!(!is_spa_text_ready(0));
        assert!(!is_spa_text_ready(SPA_BODY_TEXT_MIN_CHARS as i64 - 1));
    }

    #[test]
    fn spa_ready_at_or_above_threshold() {
        assert!(is_spa_text_ready(SPA_BODY_TEXT_MIN_CHARS as i64));
        assert!(is_spa_text_ready(50_000));
    }

    use super::NetworkActivityTracker;

    #[test]
    fn tracker_starts_idle_after_quiet_period() {
        let t = NetworkActivityTracker::new();
        assert!(t.is_idle(0));
    }

    #[test]
    fn tracker_not_idle_with_inflight_request() {
        let t = NetworkActivityTracker::new();
        t.record_request_start();
        assert!(!t.is_idle(0));
        t.record_request_end();
        assert!(t.is_idle(0));
    }

    #[test]
    fn tracker_not_idle_during_quiet_period() {
        let t = NetworkActivityTracker::new();
        t.record_request_start();
        t.record_request_end();
        // Just-ended; quiet_ms=10_000 won't have elapsed yet.
        assert!(!t.is_idle(10_000));
    }

    #[test]
    fn tracker_treats_negative_inflight_as_idle() {
        let t = NetworkActivityTracker::new();
        // Simulate a `loadingFinished` whose `requestWillBeSent` was missed
        // during pump startup. Counter goes -1, but no real work is in
        // flight — quiet-period idle should still hold.
        t.record_request_end();
        assert!(t.is_idle(0));
    }

    use super::is_browserless_direct_ws;

    #[test]
    fn browserless_token_url_is_direct_ws() {
        assert!(is_browserless_direct_ws(
            "wss://chrome.browserless.io/chromium?token=abc"
        ));
        assert!(is_browserless_direct_ws("wss://example.com/cdp?token=xyz"));
    }

    #[test]
    fn browserless_named_path_is_direct_ws() {
        assert!(is_browserless_direct_ws("wss://x.example/chromium"));
        assert!(is_browserless_direct_ws("wss://x.example/firefox"));
        assert!(is_browserless_direct_ws("wss://x.example/webkit"));
    }

    #[test]
    fn plain_lightpanda_url_is_not_direct_ws() {
        assert!(!is_browserless_direct_ws("ws://lightpanda:9222"));
        assert!(!is_browserless_direct_ws("ws://chrome:9222"));
    }

    use super::is_capturable_mime;

    #[test]
    fn capturable_mime_recognises_json_variants() {
        assert!(is_capturable_mime("application/json"));
        assert!(is_capturable_mime("application/json; charset=utf-8"));
        assert!(is_capturable_mime("application/ld+json"));
        assert!(is_capturable_mime("application/vnd.api+json"));
        assert!(is_capturable_mime("text/json"));
        assert!(is_capturable_mime("text/plain"));
    }

    #[test]
    fn capturable_mime_rejects_uninteresting_types() {
        assert!(!is_capturable_mime("text/html"));
        assert!(!is_capturable_mime("image/png"));
        assert!(!is_capturable_mime("application/octet-stream"));
        assert!(!is_capturable_mime("text/css"));
        assert!(!is_capturable_mime("application/javascript"));
        assert!(!is_capturable_mime(""));
    }

    #[test]
    fn user_agent_default_empty_and_builder_sets_it() {
        // Default = empty → fetch_inner skips the override (browser default).
        let r = CdpRenderer::new("chrome", "ws://127.0.0.1:9222", 1000, 1);
        assert_eq!(r.user_agent, "", "default UA must be empty (no override)");
        // Builder threads the effective UA through so CDP matches the HTTP path.
        let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";
        let r = r.with_user_agent(ua);
        assert_eq!(r.user_agent, ua);
    }

    #[test]
    fn lightpanda_safe_ua_strips_mozilla_keeps_chrome() {
        let full = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                    (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";
        let safe = lightpanda_safe_ua(full);
        // lightpanda's validateUserAgent rejects ANY "Mozilla" → must be gone.
        assert!(!safe.to_ascii_lowercase().contains("mozilla"));
        // ...but the Chrome version token that UA gates check must survive.
        assert!(safe.contains("Chrome/150"));
        // A UA without the prefix is returned unchanged (no double-strip).
        assert_eq!(lightpanda_safe_ua("Chrome/150.0.0.0"), "Chrome/150.0.0.0");
    }

    /// A `fetch_with_ws` cancelled mid-flight (its future dropped) must still
    /// close the browser-owned target, or the page leaks and can spin a core at
    /// 100%. This drives the guard the same way a `tokio::time::timeout` does:
    /// arm it, then drop WITHOUT `finish()`, and assert the detached reaper
    /// sent `Target.closeTarget`.
    #[tokio::test]
    async fn ws_fetch_guard_reaps_target_on_cancel() {
        use crate::cdp_conn::CdpConnection;
        use futures::StreamExt;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        // Mock CDP endpoint: record every inbound method, never reply.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = accept_async(stream).await else {
                return;
            };
            let (_w, mut r) = ws.split();
            while let Some(Ok(msg)) = r.next().await {
                if let Ok(txt) = msg.to_text()
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(txt)
                    && let Some(m) = v.get("method").and_then(|x| x.as_str())
                {
                    rec.lock().unwrap().push(m.to_string());
                }
            }
        });

        let conn = CdpConnection::connect(&format!("ws://{addr}"), Duration::from_secs(2))
            .await
            .expect("connect to mock CDP ws");

        // Arm with a recorded target, then drop without finish() = cancellation.
        let tid_slot = Arc::new(Mutex::new(Some("TARGET-1".to_string())));
        let guard = super::WsFetchGuard {
            conn: Some(conn),
            tid_slot,
            renderer: "test".to_string(),
        };
        drop(guard);

        // The detached reaper must have sent Target.closeTarget.
        for _ in 0..50 {
            if recorded
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "Target.closeTarget")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "cancellation did not reap target; recorded = {:?}",
            recorded.lock().unwrap()
        );
    }

    fn test_ctx() -> super::OutboundCtx {
        super::OutboundCtx {
            memo: Default::default(),
            render_limit: tokio::sync::Semaphore::new(super::PER_RENDER_RESOLVE_LIMIT),
            budget: std::time::Duration::from_secs(5),
            doc_host: String::new(),
            unresolved: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[tokio::test]
    async fn outbound_guard_rejects_internal_destinations() {
        let ctx = test_ctx();
        // Literal addresses the route-layer check would have rejected, arriving
        // here because a redirect or in-page navigation produced them.
        assert!(
            outbound_block_label("http://169.254.169.254/latest/meta-data/", &ctx)
                .await
                .is_some()
        );
        assert!(
            outbound_block_label("http://10.0.0.1/", &ctx)
                .await
                .is_some()
        );
        assert!(
            outbound_block_label("http://192.168.1.1/admin", &ctx)
                .await
                .is_some()
        );
        assert!(
            outbound_block_label("http://[::1]:8080/", &ctx)
                .await
                .is_some()
        );
        assert!(
            outbound_block_label("http://localhost:5432/", &ctx)
                .await
                .is_some()
        );
        // Unparseable is anomalous; fail closed.
        assert!(outbound_block_label("not a url", &ctx).await.is_some());
    }

    #[tokio::test]
    async fn outbound_guard_allows_ordinary_and_non_network_requests() {
        let ctx = test_ctx();
        // No assertion on a real public host: that path resolves DNS and the
        // check fails closed, so it would fail on an offline runner rather than
        // testing anything. The reject cases above are all literal or
        // deny-listed hosts and need no lookup.
        //
        // Never reaches a socket, and `validate_safe_url` would reject the
        // scheme, so it has to short-circuit before that check.
        assert!(
            outbound_block_label("data:image/png;base64,iVBORw0KGgo=", &ctx)
                .await
                .is_none()
        );
        assert!(
            outbound_block_label("blob:https://example.com/1234", &ctx)
                .await
                .is_none()
        );
    }
}
