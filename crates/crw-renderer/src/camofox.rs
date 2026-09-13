//! Camofox renderer tier — drives the `camofox-browser` REST server
//! (`redf0x1/camofox-browser`, default port 9377) which wraps the Camoufox
//! (Firefox) anti-detect browser behind plain HTTP.
//!
//! Firefox does not speak CDP, so this tier does NOT use the `cdp` module.
//! It is a pure-`reqwest` client: per fetch it creates a tab, waits for the
//! page to settle, evaluates `document.documentElement.outerHTML`, and closes
//! the tab. It implements the same [`PageFetcher`] trait as the CDP renderers
//! so it slots into `FallbackRenderer`'s failover ladder unchanged.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crw_core::Deadline;
use crw_core::error::{CrwError, CrwResult};
use crw_core::types::FetchResult;
use serde::Deserialize;
use serde_json::json;

use crate::traits::PageFetcher;

/// Stable `userId` for all sessions opened by one renderer instance. The
/// camofox-browser server keys an isolated Firefox profile per `userId`, so a
/// constant value lets the browser reuse one warm profile across fetches.
const USER_ID: &str = "crw";

/// Browser-context key. `/tabs` requires both `userId` and `sessionKey`. A
/// fixed key reuses one context (tabs are created and deleted per fetch, so the
/// context never accumulates tabs and sessions don't leak toward MAX_SESSIONS).
const SESSION_KEY: &str = "render";

/// JS evaluated to extract the fully-rendered DOM after navigation.
const OUTER_HTML_EXPR: &str = "document.documentElement.outerHTML";

/// Grace budget for the best-effort tab cleanup DELETE. Deliberately NOT tied to
/// the request deadline: cleanup runs after the deadline may already be spent
/// (e.g. an evaluate that timed out), and a leaked tab drives the camofox
/// context toward MAX_SESSIONS, so the reap must still get a real chance to run.
const CLEANUP_BUDGET: Duration = Duration::from_secs(3);

/// `POST /tabs` fails transiently right after the context's last tab was
/// closed: camofox eagerly tears the context down and relaunches it, and a
/// create landing in that window fails with `window is null` (or, once the
/// breaker trips, `browser has been closed`). A relaunch takes a few seconds,
/// so a 5xx create is retried this many times in total with a growing pause
/// ([`CREATE_TAB_BACKOFF`] doubling each time), inside the request deadline.
/// The tab is created blank and navigated separately: camofox counts a failed
/// navigate-in-create toward its consecutive-failure breaker (3 by default),
/// so retrying creates that also navigate would trip the breaker faster.
const CREATE_TAB_ATTEMPTS: u32 = 4;
const CREATE_TAB_BACKOFF: Duration = Duration::from_millis(500);

/// Renderer backed by a camofox-browser REST endpoint.
pub struct CamofoxRenderer {
    name: String,
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
    /// Serializes `POST /tabs`. Concurrent creates on a freshly (re)launched
    /// context race for camofox's reusable initial blank page and abort each
    /// other's navigation (`NS_BINDING_ABORTED`). A blank create is
    /// milliseconds when the context is warm, so holding this only across the
    /// create call costs nothing in steady state; navigation itself runs
    /// concurrently.
    create_lock: tokio::sync::Mutex<()>,
}

/// `POST /tabs` response — we only need the tab id.
#[derive(Deserialize)]
struct CreateTabResponse {
    #[serde(rename = "tabId")]
    tab_id: String,
}

/// `POST /tabs/:id/evaluate` response.
#[derive(Deserialize)]
struct EvaluateResponse {
    result: Option<String>,
}

/// `GET /health` response.
#[derive(Deserialize)]
struct HealthResponse {
    #[serde(rename = "browserConnected")]
    browser_connected: bool,
}

impl CamofoxRenderer {
    /// Build a renderer pointed at `base_url` (e.g. `http://camofox:9377`).
    /// `api_key`, when set, is sent as `Authorization: Bearer`. `timeout` caps
    /// each individual HTTP round-trip to the camofox-browser server.
    pub fn new(name: &str, base_url: &str, api_key: Option<String>, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|e| {
                tracing::error!("camofox: failed to build HTTP client: {e}; using default");
                reqwest::Client::new()
            });
        Self {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client,
            create_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Attach the bearer header when an API key is configured.
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }

    async fn post_json(&self, path: &str, body: serde_json::Value) -> CrwResult<reqwest::Response> {
        self.auth(self.client.post(format!("{}{path}", self.base_url)))
            .json(&body)
            .send()
            .await
            .map_err(|e| CrwError::RendererError(format!("camofox {path} request failed: {e}")))
    }

    /// Open a blank tab, retrying a 5xx create (see [`CREATE_TAB_ATTEMPTS`]).
    /// Each attempt's send + decode is bounded by the remaining deadline; a
    /// non-5xx failure surfaces at once. Creates are serialized on
    /// [`Self::create_lock`].
    async fn create_tab(&self, deadline: Deadline) -> CrwResult<String> {
        let _serialized = self.create_lock.lock().await;
        let body = json!({ "userId": USER_ID, "sessionKey": SESSION_KEY });
        let mut attempt = 1;
        let mut backoff = CREATE_TAB_BACKOFF;
        loop {
            let budget = deadline.remaining();
            if budget.is_zero() {
                return Err(CrwError::Timeout(0));
            }
            let can_retry = attempt < CREATE_TAB_ATTEMPTS;
            let fut = async {
                let resp = self.post_json("/tabs", body.clone()).await?;
                let status = resp.status();
                if status.is_success() {
                    return resp
                        .json::<CreateTabResponse>()
                        .await
                        .map(|r| Ok(r.tab_id))
                        .map_err(|e| {
                            CrwError::RendererError(format!("camofox /tabs bad response: {e}"))
                        });
                }
                let detail = error_detail(resp).await;
                if status.is_server_error() && can_retry {
                    return Ok(Err(format!("{status}{detail}")));
                }
                Err(CrwError::RendererError(format!(
                    "camofox /tabs returned {status}{detail}"
                )))
            };
            match tokio::time::timeout(budget, fut).await {
                Ok(Ok(Ok(tab_id))) => return Ok(tab_id),
                Ok(Ok(Err(transient))) => {
                    tracing::info!(
                        attempt,
                        error = %transient,
                        "camofox: tab create failed, retrying"
                    );
                    tokio::time::sleep(backoff.min(deadline.remaining())).await;
                    backoff *= 2;
                    attempt += 1;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(CrwError::Timeout(budget.as_millis() as u64)),
            }
        }
    }

    /// Navigate an open tab to `url`, send + decode bounded by the remaining
    /// deadline. A non-2xx answer carries camofox's message.
    async fn navigate_tab(&self, tab_id: &str, url: &str, deadline: Deadline) -> CrwResult<()> {
        let budget = deadline.remaining();
        if budget.is_zero() {
            return Err(CrwError::Timeout(0));
        }
        let path = format!("/tabs/{tab_id}/navigate");
        let fut = async {
            let resp = self
                .post_json(&path, json!({ "userId": USER_ID, "url": url }))
                .await?;
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            let detail = error_detail(resp).await;
            Err(CrwError::RendererError(format!(
                "camofox {path} returned {status}{detail}"
            )))
        };
        match tokio::time::timeout(budget, fut).await {
            Ok(r) => r,
            Err(_) => Err(CrwError::Timeout(budget.as_millis() as u64)),
        }
    }

    /// Best-effort `DELETE /tabs/{id}` — never fails the caller. Uses a fixed
    /// grace budget (NOT the deadline, which may already be spent) so a tab
    /// opened above is still reaped instead of leaking toward MAX_SESSIONS.
    /// Deadline expiry is the common trigger for this path.
    async fn close_tab(&self, tab_id: &str) {
        let _ = tokio::time::timeout(
            CLEANUP_BUDGET,
            self.auth(
                self.client
                    .delete(format!("{}/tabs/{tab_id}", self.base_url)),
            )
            .json(&json!({ "userId": USER_ID }))
            .send(),
        )
        .await;
    }

    /// Fire-and-discard POST bounded by `budget`. The response is dropped
    /// unread, so only the request send is bounded — used for `/wait`, whose
    /// body we never decode. The client's own `timeout` is a fixed per-op
    /// ceiling (config `chrome_timeout`, commonly 30s) far longer than a tight
    /// scrape deadline; without this each round-trip could run for that full
    /// ceiling and blow past the caller's deadline (the `PageFetcher` contract).
    /// Returns `Timeout` when the budget is already spent or the call outlives it.
    async fn post_discard_within(
        &self,
        path: &str,
        body: serde_json::Value,
        budget: Duration,
    ) -> CrwResult<()> {
        if budget.is_zero() {
            return Err(CrwError::Timeout(0));
        }
        match tokio::time::timeout(budget, self.post_json(path, body)).await {
            Ok(r) => r.map(|_| ()),
            Err(_) => Err(CrwError::Timeout(budget.as_millis() as u64)),
        }
    }

    /// POST and decode the JSON body, the WHOLE round-trip (send, status check,
    /// body read) bounded by `budget`. Bounding only the send would let a
    /// stalled response body still overrun the deadline, so the decode is inside
    /// the timeout too. Returns `Timeout` when the budget is spent or exceeded.
    async fn post_decode_within<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
        budget: Duration,
    ) -> CrwResult<T> {
        if budget.is_zero() {
            return Err(CrwError::Timeout(0));
        }
        let fut = async {
            let resp = self.post_json(path, body).await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let detail = error_detail(resp).await;
                return Err(CrwError::RendererError(format!(
                    "camofox {path} returned {status}{detail}"
                )));
            }
            resp.json::<T>()
                .await
                .map_err(|e| CrwError::RendererError(format!("camofox {path} bad response: {e}")))
        };
        match tokio::time::timeout(budget, fut).await {
            Ok(r) => r,
            Err(_) => Err(CrwError::Timeout(budget.as_millis() as u64)),
        }
    }
}

/// Cap on how much of camofox's `error` message is carried into the error.
const ERROR_BODY_CAP: usize = 300;

/// `: <message>` from a failed camofox response, or `""` when there is none.
/// camofox reports the real cause in the body's `error` field (e.g. a
/// profile/Camoufox version mismatch); only that field passes through, a
/// non-JSON body (a proxy's HTML page) is logged, not surfaced, since renderer
/// errors reach API responses.
async fn error_detail(resp: reqwest::Response) -> String {
    let status = resp.status().as_u16();
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(status, error = %e, "camofox: error body unreadable");
            String::new()
        }
    };
    let msg = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_string))
        .unwrap_or_else(|| {
            if !raw.trim().is_empty() {
                tracing::debug!(status, body = %raw.trim(), "camofox: non-JSON error body");
            }
            String::new()
        });
    let msg: String = msg.trim().chars().take(ERROR_BODY_CAP).collect();
    if msg.is_empty() {
        String::new()
    } else {
        format!(": {msg}")
    }
}

#[async_trait]
impl PageFetcher for CamofoxRenderer {
    async fn fetch(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        wait_for_ms: Option<u64>,
        deadline: Deadline,
    ) -> CrwResult<FetchResult> {
        if deadline.expired() {
            return Err(CrwError::RendererError(format!(
                "camofox: deadline expired before fetch of {url}"
            )));
        }
        let start = Instant::now();

        // 1. Open a tab navigated at `url`. Send + body decode bounded by the
        //    request budget so a stalled navigate cannot overrun the deadline.
        //    NOTE: if create succeeds server-side but the response times out here
        //    we never learn `tab_id`, so that one tab can leak until camofox
        //    idle-evicts it. Eliminating that race needs the warm-tab+mutex model
        //    the search client uses (crw-search::camofox_search); tracked as the
        //    next step, out of scope for the deadline fix.
        let tab_id = self.create_tab(deadline).await?;
        if let Err(e) = self.navigate_tab(&tab_id, url, deadline).await {
            self.close_tab(&tab_id).await;
            return Err(e);
        }

        // 2. Wait for readiness, bounded by the smaller of the caller's
        //    `wait_for_ms` hint and the remaining request budget. The HTTP call
        //    itself is capped at the remaining budget too, so a server-side wait
        //    that ignores its `timeout` can't overrun the deadline.
        let budget_ms = deadline.remaining().as_millis() as u64;
        let wait_ms = wait_for_ms.unwrap_or(budget_ms).min(budget_ms);
        let _ = self
            .post_discard_within(
                &format!("/tabs/{tab_id}/wait"),
                json!({ "userId": USER_ID, "timeout": wait_ms }),
                deadline.remaining(),
            )
            .await;

        // 3. Evaluate the rendered DOM, send + body decode bounded by the budget.
        let html = self
            .post_decode_within::<EvaluateResponse>(
                &format!("/tabs/{tab_id}/evaluate"),
                json!({ "userId": USER_ID, "expression": OUTER_HTML_EXPR }),
                deadline.remaining(),
            )
            .await;

        // 4. Best-effort close — never fail the fetch on cleanup.
        self.close_tab(&tab_id).await;

        let html = html?.result.unwrap_or_default();
        if html.is_empty() {
            return Err(CrwError::RendererError(
                "camofox: evaluate returned empty document".to_string(),
            ));
        }

        // The camofox-browser REST API exposes only `tabId` and the evaluated
        // `result` — it returns no navigation status code, final URL, or response
        // content-type. So these three are best-effort synthetic values, NOT
        // observed from the wire: a camofox-rendered 404 or redirect is reported
        // here as a 200. Downstream anti-bot/block classification still runs on
        // the returned `html` (see crw_crawl::single::classify_block), which is
        // the real signal for this tier; surfacing true status/final_url needs an
        // API that returns them.
        Ok(FetchResult {
            url: url.to_string(),
            final_url: None,
            status_code: 200,
            html,
            content_type: Some("text/html".to_string()),
            raw_bytes: None,
            rendered_with: Some("camofox".to_string()),
            elapsed_ms: start.elapsed().as_millis() as u64,
            warning: None,
            render_decision: None,
            credit_cost: 0,
            warnings: Vec::new(),
            truncated: false,
            deadline_exceeded: deadline.expired(),
            captured_responses: Vec::new(),
        })
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn supports_js(&self) -> bool {
        true
    }

    async fn is_available(&self) -> bool {
        let req = self.auth(self.client.get(format!("{}/health", self.base_url)));
        match req.send().await {
            Ok(resp) if resp.status().is_success() => resp
                .json::<HealthResponse>()
                .await
                .map(|h| h.browser_connected)
                .unwrap_or(false),
            _ => false,
        }
    }
}
