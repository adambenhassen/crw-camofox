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

/// Renderer backed by a camofox-browser REST endpoint.
pub struct CamofoxRenderer {
    name: String,
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
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

        // 1. Open a tab navigated at `url`.
        let create = self
            .post_json(
                "/tabs",
                json!({ "userId": USER_ID, "sessionKey": SESSION_KEY, "url": url }),
            )
            .await?;
        if !create.status().is_success() {
            return Err(CrwError::RendererError(format!(
                "camofox: create tab returned {}",
                create.status()
            )));
        }
        let tab_id = create
            .json::<CreateTabResponse>()
            .await
            .map_err(|e| CrwError::RendererError(format!("camofox: bad /tabs response: {e}")))?
            .tab_id;

        // 2. Wait for readiness, bounded by the smaller of the caller's
        //    `wait_for_ms` hint and the remaining request budget.
        let budget_ms = deadline.remaining().as_millis() as u64;
        let wait_ms = wait_for_ms.unwrap_or(budget_ms).min(budget_ms);
        let _ = self
            .post_json(
                &format!("/tabs/{tab_id}/wait"),
                json!({ "userId": USER_ID, "timeout": wait_ms }),
            )
            .await;

        // 3. Evaluate the rendered DOM.
        let html = async {
            let resp = self
                .post_json(
                    &format!("/tabs/{tab_id}/evaluate"),
                    json!({ "userId": USER_ID, "expression": OUTER_HTML_EXPR }),
                )
                .await?;
            resp.json::<EvaluateResponse>().await.map_err(|e| {
                CrwError::RendererError(format!("camofox: bad evaluate response: {e}"))
            })
        }
        .await;

        // 4. Best-effort close — never fail the fetch on cleanup.
        let _ = self
            .auth(
                self.client
                    .delete(format!("{}/tabs/{tab_id}", self.base_url)),
            )
            .json(&json!({ "userId": USER_ID }))
            .send()
            .await;

        let html = html?.result.unwrap_or_default();
        if html.is_empty() {
            return Err(CrwError::RendererError(
                "camofox: evaluate returned empty document".to_string(),
            ));
        }

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
