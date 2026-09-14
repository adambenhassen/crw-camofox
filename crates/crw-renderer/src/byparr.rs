//! Byparr challenge-solver tier — drives a Byparr server
//! (`ThePhaseless/Byparr`, default port 8191), a FlareSolverr-compatible REST
//! API around a stealth Firefox that clicks the Cloudflare Turnstile checkbox.
//!
//! One `POST /v1` per fetch: Byparr opens a fresh browser, navigates, solves any
//! challenge, and returns the page HTML, the final URL, the cookie jar and the
//! user agent. The ladder only calls this tier after an earlier attempt came
//! back as an anti-bot challenge (see `FallbackRenderer::fetch_with_js`), since
//! every solve costs a browser launch.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use crw_core::Deadline;
use crw_core::error::{CrwError, CrwResult};
use crw_core::types::FetchResult;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::clearance::{Clearance, ClearanceCache, Cookie, cookie_matches_host};
use crate::traits::PageFetcher;

/// Held back from the solve budget for the round-trip around it: request and
/// response transfer, and the final-URL check afterwards.
const ROUND_TRIP_OVERHEAD: Duration = Duration::from_secs(2);

/// Below this a solve cannot finish (a browser launch alone takes seconds).
const MIN_SOLVE_BUDGET: Duration = Duration::from_secs(5);

/// Byparr reads `maxTimeout` below 1000 as seconds, so never send less.
const MIN_MAX_TIMEOUT_MS: u64 = 1_000;

/// Cap on how much of Byparr's error `detail` is carried into the error.
const ERROR_BODY_CAP: usize = 300;

/// Budget for the availability probe.
const AVAILABILITY_TIMEOUT: Duration = Duration::from_secs(5);

/// `POST /v1` response.
#[derive(Deserialize)]
struct SolveResponse {
    status: String,
    #[serde(default)]
    message: String,
    solution: Option<Solution>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Solution {
    url: String,
    #[serde(default)]
    cookies: Vec<Cookie>,
    #[serde(default)]
    user_agent: String,
    #[serde(default)]
    response: String,
    #[serde(default)]
    content_type: Option<String>,
}

/// Renderer backed by a Byparr endpoint.
pub struct ByparrRenderer {
    base_url: String,
    client: reqwest::Client,
    /// Longest one solve may take (config `timeout_ms`).
    timeout: Duration,
    /// Concurrent solves (config `max_concurrent`); each launches a browser.
    permits: Semaphore,
    /// Where a `cf_clearance` earned by a solve is stored for the HTTP tier.
    clearance: Option<Arc<ClearanceCache>>,
}

impl ByparrRenderer {
    /// Build a renderer pointed at `base_url` (e.g. `http://byparr:8191`).
    pub fn new(base_url: &str, timeout: Duration, max_concurrent: usize) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            timeout,
            permits: Semaphore::new(max_concurrent.max(1)),
            clearance: None,
        }
    }

    /// Enable clearance capture into `cache` (config `clearance_reuse`).
    pub fn with_clearance_cache(mut self, cache: Arc<ClearanceCache>) -> Self {
        self.clearance = Some(cache);
        self
    }

    /// One solve, the whole round-trip bounded by `budget` plus the overhead.
    async fn solve(&self, url: &str, budget: Duration) -> CrwResult<Solution> {
        let max_timeout_ms = (budget.as_millis() as u64).max(MIN_MAX_TIMEOUT_MS);
        let body = json!({ "cmd": "request.get", "url": url, "maxTimeout": max_timeout_ms });
        let round_trip = budget + ROUND_TRIP_OVERHEAD;
        let fut = async {
            let resp = self
                .client
                .post(format!("{}/v1", self.base_url))
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    // The Byparr URL is internal; log it, return the stripped message.
                    tracing::warn!("byparr /v1 request failed: {e}");
                    CrwError::RendererError(format!(
                        "byparr /v1 request failed: {}",
                        crw_core::error::reqwest_message(e)
                    ))
                })?;
            let status = resp.status();
            if !status.is_success() {
                let detail = error_detail(resp).await;
                return Err(match status.as_u16() {
                    408 => CrwError::Timeout(max_timeout_ms),
                    // "Could not reach the target": the origin, not Byparr. The
                    // ladder reads "navigation failed" as an origin failure.
                    502 => CrwError::RendererError(format!(
                        "byparr: navigation failed, page did not load{detail}"
                    )),
                    _ => CrwError::RendererError(format!("byparr /v1 returned {status}{detail}")),
                });
            }
            let parsed = resp.json::<SolveResponse>().await.map_err(|e| {
                CrwError::RendererError(format!(
                    "byparr /v1 bad response: {}",
                    crw_core::error::reqwest_message(e)
                ))
            })?;
            match parsed.solution {
                Some(solution) if parsed.status == "ok" => Ok(solution),
                _ => Err(CrwError::RendererError(format!(
                    "byparr /v1 reported {}: {}",
                    parsed.status,
                    parsed
                        .message
                        .chars()
                        .take(ERROR_BODY_CAP)
                        .collect::<String>()
                ))),
            }
        };
        match tokio::time::timeout(round_trip, fut).await {
            Ok(r) => r,
            Err(_) => Err(CrwError::Timeout(round_trip.as_millis() as u64)),
        }
    }

    /// Refuse a page whose final URL is internal. The route layer checked the
    /// URL the caller gave, but a redirect inside the browser can land on the
    /// metadata endpoint or a compose service, and Byparr renders whatever it
    /// lands on. Byparr's own requests along the way are not covered; only
    /// network-level egress rules can do that.
    async fn check_final_url(&self, final_url: &str, deadline: Deadline) -> CrwResult<()> {
        let Ok(parsed) = url::Url::parse(final_url) else {
            return Err(CrwError::RendererError(
                "byparr: could not read the page's final URL".to_string(),
            ));
        };
        let verdict = if matches!(parsed.scheme(), "http" | "https") {
            match tokio::time::timeout(
                deadline.remaining(),
                crw_core::url_safety::classify_safe_host_resolved(&parsed),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => return Err(CrwError::Timeout(deadline.requested_ms())),
            }
        } else {
            Err(crw_core::url_safety::HostRejection::Policy(
                "scheme".to_string(),
            ))
        };
        match verdict {
            Ok(()) => Ok(()),
            Err(crw_core::url_safety::HostRejection::Policy(_)) => {
                crw_core::metrics::metrics()
                    .chrome_blocked_requests_total
                    .with_label_values(&["byparr_final_url"])
                    .inc();
                tracing::warn!("byparr: the page navigated to a blocked destination");
                Err(CrwError::RendererError(
                    "byparr: the page navigated to a blocked destination".to_string(),
                ))
            }
            // Our resolver could not answer. Fail closed, and say it is ours.
            Err(crw_core::url_safety::HostRejection::Unresolved(reason)) => {
                Err(CrwError::RendererError(format!(
                    "byparr: outbound destination check unavailable ({reason})"
                )))
            }
        }
    }

    /// Store this host's cookies + the user agent when the solve earned a
    /// `cf_clearance`. Byparr launches a fresh browser per solve, but the jar
    /// can still hold third-party cookies, so only the host's are kept.
    async fn capture_clearance(&self, url: &str, solution: &Solution) {
        let Some(cache) = &self.clearance else {
            return;
        };
        if solution.user_agent.is_empty() {
            return;
        }
        let Some(host) = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
        else {
            return;
        };
        let cookies: Vec<Cookie> = solution
            .cookies
            .iter()
            .filter(|c| !c.domain.trim().is_empty() && cookie_matches_host(&c.domain, &host))
            .cloned()
            .collect();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if let Some(clearance) =
            Clearance::from_browser(cookies, solution.user_agent.clone(), now_unix)
        {
            tracing::info!(host = %host, "byparr: cached cf_clearance for the HTTP tier");
            cache.insert(&host, clearance).await;
        }
    }
}

/// `": <detail>"` from a FastAPI error body, or empty.
async fn error_detail(resp: reqwest::Response) -> String {
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(error = %e, "byparr: error body unreadable");
            return String::new();
        }
    };
    let msg = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("detail")?.as_str().map(str::to_string))
        .unwrap_or_default();
    let msg: String = msg.trim().chars().take(ERROR_BODY_CAP).collect();
    if msg.is_empty() {
        String::new()
    } else {
        format!(": {msg}")
    }
}

#[async_trait]
impl PageFetcher for ByparrRenderer {
    async fn fetch(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        _wait_for_ms: Option<u64>,
        deadline: Deadline,
    ) -> CrwResult<FetchResult> {
        let start = Instant::now();
        let _permit = match tokio::time::timeout(deadline.remaining(), self.permits.acquire()).await
        {
            Ok(Ok(p)) => p,
            Ok(Err(_)) => return Err(CrwError::RendererError("byparr: limiter closed".into())),
            Err(_) => return Err(CrwError::Timeout(deadline.requested_ms())),
        };
        let budget = self
            .timeout
            .min(deadline.remaining().saturating_sub(ROUND_TRIP_OVERHEAD));
        if budget < MIN_SOLVE_BUDGET {
            return Err(CrwError::Timeout(deadline.requested_ms()));
        }

        let solution = self.solve(url, budget).await?;
        self.check_final_url(&solution.url, deadline).await?;
        if solution.response.trim().is_empty() {
            return Err(CrwError::RendererError(
                "byparr: solve returned an empty document".to_string(),
            ));
        }
        self.capture_clearance(url, &solution).await;

        Ok(FetchResult {
            url: url.to_string(),
            final_url: (solution.url != url).then(|| solution.url.clone()),
            // Byparr reports 200 for every page it returns; the ladder's body
            // checks classify what is actually on it.
            status_code: 200,
            html: solution.response,
            content_type: solution
                .content_type
                .or_else(|| Some("text/html".to_string())),
            raw_bytes: None,
            rendered_with: Some("byparr".to_string()),
            elapsed_ms: start.elapsed().as_millis() as u64,
            warning: None,
            render_decision: None,
            credit_cost: 0,
            warnings: Vec::new(),
            wall: None,
            truncated: false,
            deadline_exceeded: deadline.expired(),
            captured_responses: Vec::new(),
        })
    }

    fn name(&self) -> &str {
        "byparr"
    }

    fn supports_js(&self) -> bool {
        true
    }

    /// `GET /docs`. Byparr's own `/health` launches a browser and loads a
    /// public page, far too heavy for a readiness probe.
    async fn is_available(&self) -> bool {
        matches!(
            tokio::time::timeout(
                AVAILABILITY_TIMEOUT,
                self.client.get(format!("{}/docs", self.base_url)).send(),
            )
            .await,
            Ok(Ok(resp)) if resp.status().is_success()
        )
    }
}
