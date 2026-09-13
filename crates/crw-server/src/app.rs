use axum::Router;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// Maximum request body size (1 MB) to prevent memory exhaustion from large payloads.
const MAX_BODY_SIZE: usize = 1024 * 1024;

use crate::middleware::auth_middleware;
use crate::routes::{self, method_not_allowed};
use crate::state::AppState;

pub fn create_app(state: AppState) -> Router {
    let api_keys = Arc::new(state.config.auth.api_keys.clone());
    // Tower outer timeout. `effective_request_timeout_secs()` widens the
    // operator baseline so the longest legitimate handler runtime (auto-extended
    // scrape, search enrichment fan-out, map's 300s ceiling) isn't cancelled
    // by the outer layer before the inner deadline fires. See issue #35.
    let timeout = Duration::from_secs(state.config.effective_request_timeout_secs());
    let rate_limit_rps = state.config.server.rate_limit_rps;
    let cors_origins = state.config.server.cors_allowed_origins.clone();

    // v1 + v2 routers merged before the shared auth + rate-limit layers, so the
    // /v2/* surface (issue #62) inherits auth, rate-limiting, body-limit and the
    // timeout layer identically to v1. `/mcp` is version-less.
    let api_routes = routes::v1::router()
        .merge(routes::v2::router())
        .route(
            "/mcp",
            post(routes::mcp::mcp_handler).fallback(method_not_allowed),
        )
        // Ops + admin surfaces live INSIDE the auth boundary. When
        // `[auth].api_keys` is set they require a valid Bearer token; with no
        // keys configured (default self-host) they stay open, exactly like the
        // scraper API. Previously these were mounted on the base router below,
        // bypassing `auth_middleware` even on key-secured deployments.
        .route(
            "/metrics",
            get(routes::metrics::metrics).fallback(method_not_allowed),
        )
        .route(
            "/metrics/renderer-breakers",
            get(routes::breakers::renderer_breakers).fallback(method_not_allowed),
        )
        .route(
            "/admin/breakers/reset",
            post(routes::breakers::reset_breakers).fallback(method_not_allowed),
        );

    let api_routes = if api_keys.is_empty() {
        api_routes.with_state(state.clone())
    } else {
        api_routes
            .route_layer(axum::middleware::from_fn_with_state(
                api_keys,
                auth_middleware,
            ))
            .with_state(state.clone())
    };

    let rate_limiter = if rate_limit_rps > 0 {
        Some(Arc::new(RateLimiter::new(rate_limit_rps)))
    } else {
        None
    };

    // Base router: only liveness + public schema stay OUTSIDE the auth boundary.
    // Ops/admin routes now live in `api_routes` above. `health`/`ready` take
    // `State<AppState>`, so `.with_state(state)` still belongs here.
    let app = Router::new()
        .route(
            "/health",
            get(routes::health::health).fallback(method_not_allowed),
        )
        .route(
            "/openapi.json",
            get(routes::openapi::serve_openapi_3_1).fallback(method_not_allowed),
        )
        .route(
            "/openapi-3.0.json",
            get(routes::openapi::serve_openapi_3_0).fallback(method_not_allowed),
        )
        .route(
            "/ready",
            get(routes::health::ready).fallback(method_not_allowed),
        )
        .with_state(state)
        .merge(api_routes)
        .layer(axum::middleware::from_fn(move |req, next| {
            let limiter = rate_limiter.clone();
            rate_limit_middleware(limiter, req, next)
        }))
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            timeout,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ));

    // CORS is opt-in via `server.cors_allowed_origins`. Empty (default) = no
    // layer at all, so no `Access-Control-Allow-Origin` is emitted and browsers
    // block cross-origin JS reads — the safe default for a server-to-server API,
    // replacing the old blanket `CorsLayer::permissive()`. (The `/openapi*`
    // handlers still set `ACAO: *` themselves — an intentional public-schema
    // exception unaffected by this layer.)
    let app = match build_cors_layer(&cors_origins) {
        Some(cors) => app.layer(cors),
        None => app,
    };

    app.layer(TraceLayer::new_for_http())
}

/// Build a CORS layer from the configured origin allowlist. Returns `None` when
/// the allowlist is empty (or yields no valid origin), so the caller omits the
/// layer entirely and no CORS headers are emitted. Only explicit
/// `scheme://host` origins are accepted; `*`, `null`, and schemeless entries are
/// rejected (see the inline notes) so the setting can never silently widen back
/// into a permissive or opaque-origin allowance.
fn build_cors_layer(origins: &[String]) -> Option<CorsLayer> {
    let allow: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| {
            let origin = origin.trim();
            if origin.is_empty() {
                return None;
            }
            // Reject footguns before building the allowlist entry:
            //   `*`    — the wildcard permissive default this setting replaces;
            //            `AllowOrigin::list(["*"])` also panics in tower-http.
            //   `null` — the opaque-origin token; allowing it grants access to
            //            sandboxed iframes and `file://` contexts.
            //   no `://` — a real Origin is `scheme://host[:port]`, so a
            //            schemeless entry can never match and would silently
            //            leave CORS disabled.
            let lower = origin.to_ascii_lowercase();
            if lower == "*" || lower == "null" || !origin.contains("://") {
                tracing::warn!(
                    origin,
                    "server.cors_allowed_origins: not a valid explicit origin \
                     (expected scheme://host, never `*` or `null`); ignoring entry"
                );
                return None;
            }
            // Browsers send the origin serialization: lowercase scheme and host,
            // no path. Normalize to it, or an entry written with a trailing slash
            // or capitals never matches and CORS stays off without a word.
            let serialized = match url::Url::parse(origin) {
                Ok(u) if matches!(u.path(), "" | "/") && u.query().is_none() => {
                    u.origin().ascii_serialization()
                }
                _ => {
                    tracing::warn!(
                        origin,
                        "server.cors_allowed_origins: not an origin (expected \
                         scheme://host[:port] with no path); ignoring entry"
                    );
                    return None;
                }
            };
            match HeaderValue::from_str(&serialized) {
                Ok(value) if serialized != "null" => Some(value),
                _ => {
                    tracing::warn!(
                        origin,
                        "server.cors_allowed_origins: invalid origin, ignoring"
                    );
                    None
                }
            }
        })
        .collect();

    if allow.is_empty() {
        if !origins.is_empty() {
            // Configured but nothing survived (all blank/`*`/malformed) — warn so
            // an operator who fat-fingered their only origin doesn't get a
            // silently CORS-less server.
            tracing::warn!(
                "server.cors_allowed_origins was set but yielded no valid origin; no CORS layer applied"
            );
        }
        return None;
    }

    Some(
        CorsLayer::new()
            .allow_origin(allow)
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
}

/// Simple token-bucket rate limiter using atomic counters.
/// Refills `rps` tokens every second.
struct RateLimiter {
    tokens: AtomicU64,
    max_tokens: u64,
    last_refill: std::sync::Mutex<std::time::Instant>,
}

impl RateLimiter {
    fn new(rps: u64) -> Self {
        Self {
            tokens: AtomicU64::new(rps),
            max_tokens: rps,
            last_refill: std::sync::Mutex::new(std::time::Instant::now()),
        }
    }

    fn try_acquire(&self) -> bool {
        // Refill tokens based on elapsed time.
        {
            let mut last = self.last_refill.lock().unwrap();
            let elapsed = last.elapsed();
            if elapsed >= Duration::from_secs(1) {
                let refill = (elapsed.as_secs_f64() * self.max_tokens as f64) as u64;
                let current = self.tokens.load(Ordering::Relaxed);
                let new_val = (current + refill).min(self.max_tokens);
                self.tokens.store(new_val, Ordering::Relaxed);
                *last = std::time::Instant::now();
            }
        }

        // Try to consume one token.
        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current == 0 {
                return false;
            }
            if self
                .tokens
                .compare_exchange_weak(current, current - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }
}

async fn rate_limit_middleware(
    limiter: Option<Arc<RateLimiter>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(limiter) = limiter
        && req.uri().path() != "/health"
        && req.uri().path() != "/ready"
        && !limiter.try_acquire()
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(crw_core::types::ApiResponse::<()>::err_with_code(
                "Rate limited",
                "rate_limited",
            )),
        )
            .into_response();
    }
    next.run(req).await
}
