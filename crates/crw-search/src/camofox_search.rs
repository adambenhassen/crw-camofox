//! Camofox-backed Google search client.
//!
//! Search SERPs trip anti-bot / consent walls immediately, so search does NOT
//! use the renderer failover ladder — it drives the camofox-browser (Firefox)
//! tier directly: navigate a tab with the built-in `@google_search` macro,
//! wait, then scrape the result rows via `/evaluate`.
//!
//! Concurrency: camofox-browser keys one persistent context per `userId` and
//! eagerly tears that context down when its tab count hits zero, leaving a
//! ~9s relaunch window in which `newPage` throws `window is null`. Creating
//! and deleting a tab per query raced that teardown, so concurrent or
//! rapid-sequential searches failed with empty / 5xx results. We avoid the
//! race entirely: a single [`tokio::sync::Mutex`] serializes all browser
//! access and guards ONE long-lived warm tab that is reused across queries and
//! never deleted — the context never sees concurrent tabs nor drops to zero.
//! If the warm tab goes stale (idle eviction / camofox restart) the next
//! navigate fails and we transparently recreate the tab and retry once.
//!
//! Rows are mapped into the existing [`SearxngResponse`] shape so the entire
//! downstream transform / rerank pipeline (`transform.rs`, `rerank.rs`) is
//! reused unchanged — this client is a drop-in alternative upstream source, not
//! a new result format.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::client::{SearchError, SearxngResponse, SearxngResult};
use crate::params::SearxngParams;

/// Stable `userId` for the search client's camofox sessions (separate from the
/// renderer tier's so search and scrape don't share a profile).
const USER_ID: &str = "crw-search";

/// Browser-context key. `/tabs` requires both `userId` and `sessionKey`. Fixed
/// so the context is reused across queries; combined with the single warm tab
/// (see [`CamofoxSearchClient::tab`]) the context never accumulates tabs nor
/// drops to zero, and sessions don't leak toward MAX_SESSIONS.
const SESSION_KEY: &str = "search";

/// Pause before recreating a stale tab, giving any in-flight upstream context
/// relaunch a moment to settle before we retry. Only hit on the rare
/// stale-tab path (idle eviction / camofox restart), not the steady state.
const RETRY_BACKOFF: Duration = Duration::from_millis(750);

/// JS evaluated in the Google SERP to extract result rows. Returns a JSON
/// *string* (via `JSON.stringify`) so the camofox `/evaluate` `result` field
/// comes back as a string we can parse. Selectors are intentionally broad and
/// kept in this one place — Google rewrites its SERP DOM periodically, so this
/// is the single spot to fix when extraction drifts.
const SCRAPE_JS: &str = r#"JSON.stringify(Array.from(document.querySelectorAll('div.g, div.MjjYud')).map(function(el){var a=el.querySelector('a[href]');var h=el.querySelector('h3');var s=el.querySelector('.VwiC3b, [data-sncf], .st');return (a&&h)?{url:a.href,title:h.innerText,content:s?s.innerText:''}:null;}).filter(Boolean))"#;

/// One scraped SERP row, as emitted by [`SCRAPE_JS`].
#[derive(Deserialize)]
struct ScrapedRow {
    url: String,
    title: String,
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct CreateTabResponse {
    #[serde(rename = "tabId")]
    tab_id: String,
}

#[derive(Deserialize)]
struct EvaluateResponse {
    result: Option<String>,
}

/// Search client backed by a camofox-browser REST endpoint. Returns the same
/// [`SearxngResponse`] shape as [`crate::client::SearxngClient`] so callers can
/// treat the two interchangeably.
pub struct CamofoxSearchClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    timeout: Duration,
    /// The single warm tab id, lazily created and reused across queries. The
    /// mutex doubles as the search serializer: holding it for the whole `fetch`
    /// guarantees one navigation at a time on the one shared tab. `None` until
    /// the first search creates a tab, and reset to `None` when a tab goes
    /// stale so the next search recreates it.
    tab: tokio::sync::Mutex<Option<String>>,
}

impl CamofoxSearchClient {
    /// Build a client pointed at the camofox-browser base URL
    /// (e.g. `http://camofox:9377`). `timeout` caps each HTTP round-trip.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>, timeout: Duration) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base_url,
            api_key,
            timeout,
            tab: tokio::sync::Mutex::new(None),
        }
    }

    /// Configured base URL (trailing slash trimmed). Mirrors
    /// [`SearxngClient::base_url`](crate::client::SearxngClient::base_url) so the
    /// route layer can name the host in errors uniformly.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<reqwest::Response, SearchError> {
        self.auth(self.http.post(format!("{}{path}", self.base_url)))
            .json(&body)
            .send()
            .await
            .map_err(|e: reqwest::Error| {
                if e.is_timeout() {
                    SearchError::Timeout
                } else {
                    SearchError::Transport(e.without_url().to_string())
                }
            })
    }

    /// Run a Google search via Camofox and map the rows into a
    /// [`SearxngResponse`]. Typed [`SearchError`]s match the SearXNG client so
    /// the route layer's existing error mapping applies unchanged.
    ///
    /// Serializes on the warm-tab mutex (see [`Self::tab`]) so only one search
    /// touches the browser at a time, reusing the one long-lived tab. If that
    /// tab has gone stale, recreates it and retries the search exactly once.
    pub async fn fetch(&self, params: &SearxngParams) -> Result<SearxngResponse, SearchError> {
        let mut tab = self.tab.lock().await;

        match self.attempt(&mut tab, params).await {
            Ok(resp) => Ok(resp),
            Err(e) if is_stale_tab(&e) => {
                // Warm tab/context died (idle eviction or camofox restart). Drop
                // the dead id, let any in-flight relaunch settle, then recreate
                // and retry once. A second failure is reported honestly.
                *tab = None;
                tokio::time::sleep(RETRY_BACKOFF).await;
                self.attempt(&mut tab, params).await
            }
            Err(e) => Err(e),
        }
    }

    /// Ensure a warm tab exists, then run the search against it. Caller holds
    /// the tab mutex, so this is the single in-flight search.
    async fn attempt(
        &self,
        tab: &mut Option<String>,
        params: &SearxngParams,
    ) -> Result<SearxngResponse, SearchError> {
        let tab_id = self.ensure_tab(tab).await?;
        self.run_search(&tab_id, params).await
    }

    /// Return the warm tab id, creating one if we don't have it cached. The id
    /// is cached back into `tab` so subsequent searches reuse it.
    async fn ensure_tab(&self, tab: &mut Option<String>) -> Result<String, SearchError> {
        if let Some(id) = tab.as_ref() {
            return Ok(id.clone());
        }
        let create = self
            .post("/tabs", json!({ "userId": USER_ID, "sessionKey": SESSION_KEY }))
            .await?;
        if !create.status().is_success() {
            return Err(SearchError::Upstream {
                status: create.status().as_u16(),
                body: "camofox: create tab failed".to_string(),
            });
        }
        let id = create
            .json::<CreateTabResponse>()
            .await
            .map_err(|e| SearchError::InvalidResponse(format!("camofox: bad /tabs response: {e}")))?
            .tab_id;
        *tab = Some(id.clone());
        Ok(id)
    }

    async fn run_search(
        &self,
        tab_id: &str,
        params: &SearxngParams,
    ) -> Result<SearxngResponse, SearchError> {
        let nav = self
            .post(
                &format!("/tabs/{tab_id}/navigate"),
                json!({ "userId": USER_ID, "macro": "@google_search", "query": params.q }),
            )
            .await?;
        if !nav.status().is_success() {
            return Err(SearchError::Upstream {
                status: nav.status().as_u16(),
                body: "camofox: navigate failed".to_string(),
            });
        }

        let _ = self
            .post(
                &format!("/tabs/{tab_id}/wait"),
                json!({ "userId": USER_ID, "timeout": self.timeout.as_millis() as u64 }),
            )
            .await;

        let eval = self
            .post(
                &format!("/tabs/{tab_id}/evaluate"),
                json!({ "userId": USER_ID, "expression": SCRAPE_JS }),
            )
            .await?;
        let raw = eval
            .json::<EvaluateResponse>()
            .await
            .map_err(|e| SearchError::InvalidResponse(format!("camofox: bad evaluate response: {e}")))?
            .result
            .unwrap_or_default();

        let rows: Vec<ScrapedRow> = if raw.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&raw)
                .map_err(|e| SearchError::InvalidResponse(format!("camofox: scrape JSON: {e}")))?
        };

        let n = rows.len();
        let results = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearxngResult {
                url: Some(r.url),
                title: Some(r.title),
                engine: Some("google".to_string()),
                content: (!r.content.is_empty()).then_some(r.content),
                // Synthesize a descending score from SERP position so the
                // existing score-sort in transform.rs preserves Google's order.
                score: Some((n - i) as f64),
                engines: vec!["google".to_string()],
                positions: vec![(i + 1) as u32],
                category: Some("general".to_string()),
                template: None,
                published_date: None,
                img_src: None,
                thumbnail_src: None,
                img_format: None,
                resolution: None,
            })
            .collect();

        Ok(SearxngResponse {
            query: params.q.clone(),
            number_of_results: n as u64,
            results,
            ..Default::default()
        })
    }
}

/// Whether an error means the warm tab/context is gone and recreating it could
/// recover — a missing tab (404), a server-side fault like the upstream
/// `window is null` (5xx), or a dropped connection during a relaunch. A
/// timeout or a malformed-response parse error won't be helped by recreating,
/// so they're reported as-is.
fn is_stale_tab(e: &SearchError) -> bool {
    match e {
        SearchError::Upstream { status, .. } => *status == 404 || *status >= 500,
        SearchError::Transport(_) => true,
        SearchError::Timeout | SearchError::InvalidResponse(_) => false,
    }
}
