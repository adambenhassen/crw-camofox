//! Camofox-backed web-search client.
//!
//! Search SERPs trip anti-bot / consent walls immediately, so search does NOT
//! use the renderer failover ladder — it drives the camofox-browser (Firefox)
//! tier directly: navigate a tab to the engine's SERP, wait, then scrape the
//! result rows via `/evaluate`. Google uses the browser's built-in
//! `@google_search` macro; Bing/DuckDuckGo/GitHub have no working macro in
//! camofox-browser, so we navigate their search URL directly (see
//! [`navigate_body`]). Multiple engines requested in one call run sequentially
//! on the warm tab and their rows are merged (see [`merge_results`]).
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

use crw_core::types::SearchEngine;

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
const GOOGLE_SCRAPE_JS: &str = r#"JSON.stringify(Array.from(document.querySelectorAll('div.g, div.MjjYud')).map(function(el){var a=el.querySelector('a[href]');var h=el.querySelector('h3');var s=el.querySelector('.VwiC3b, [data-sncf], .st');return (a&&h)?{url:a.href,title:h.innerText,content:s?s.innerText:''}:null;}).filter(Boolean))"#;

/// Bing SERP extractor. `li.b_algo` rows; `h2 a` for title/url, `.b_caption p`
/// for the snippet. Bing wraps result links in a `bing.com/ck/a?…&u=a1<base64>`
/// click-tracker — the inline `unwrap` decodes that `u` param back to the real
/// destination (and leaves already-direct links untouched).
const BING_SCRAPE_JS: &str = r#"JSON.stringify((function(){function unwrap(u){try{var m=u.match(/[?&]u=a1([^&]+)/);if(m){var b=m[1].replace(/-/g,'+').replace(/_/g,'/');while(b.length%4)b+='=';return decodeURIComponent(escape(atob(b)));}}catch(e){}return u;}return Array.from(document.querySelectorAll('li.b_algo')).map(function(el){var a=el.querySelector('h2 a[href]');var s=el.querySelector('.b_caption p, p');return a?{url:unwrap(a.href),title:a.innerText,content:s?s.innerText:''}:null;}).filter(Boolean);})())"#;

/// DuckDuckGo SERP extractor (the `duckduckgo.com/?q=` layout). Result blocks
/// are `article[data-testid="result"]` with `h2 a` and a snippet node; the
/// `div.result` / `a.result__a` fallbacks cover the lite/html layout.
const DDG_SCRAPE_JS: &str = r#"JSON.stringify(Array.from(document.querySelectorAll('article[data-testid="result"], div.result')).map(function(el){var a=el.querySelector('h2 a[href], a.result__a[href]');var s=el.querySelector('[data-result="snippet"], .result__snippet');return a?{url:a.href,title:a.innerText,content:s?s.innerText:''}:null;}).filter(Boolean))"#;

/// Wikipedia full-text search extractor. The `Special:Search` SERP lists hits
/// as `.mw-search-result-heading a` (absolute article hrefs, no inline snippet).
const WIKIPEDIA_SCRAPE_JS: &str = r#"JSON.stringify(Array.from(document.querySelectorAll('.mw-search-result-heading a')).map(function(a){var t=(a.innerText||'').trim();return (a.href&&t)?{url:a.href,title:t,content:''}:null;}).filter(Boolean))"#;

/// YouTube search extractor. Each video result is a `ytd-video-renderer` whose
/// `a#video-title` carries the watch URL and the full title in its `title`
/// attribute (the inner text is lazy/empty until hover).
const YOUTUBE_SCRAPE_JS: &str = r#"JSON.stringify(Array.from(document.querySelectorAll('ytd-video-renderer a#video-title')).map(function(a){var t=(a.getAttribute('title')||a.innerText||'').trim();return (a.href&&t)?{url:a.href,title:t,content:''}:null;}).filter(Boolean))"#;

/// Reddit search extractor. Post links are `a[href*="/comments/"]`; Reddit
/// renders several anchors per post (thumbnail + title), so we dedupe by the
/// query-stripped permalink and keep the first non-trivial link text.
const REDDIT_SCRAPE_JS: &str = r#"JSON.stringify((function(){var seen={};var out=[];document.querySelectorAll('a[href*="/comments/"]').forEach(function(a){var u=a.href.split('?')[0];var t=(a.innerText||'').trim();if(t.length>5&&!seen[u]){seen[u]=1;out.push({url:u,title:t,content:''});}});return out;})())"#;

/// Amazon product-search extractor. Each `[data-component-type="s-search-result"]`
/// card holds the product link (`a[href*="/dp/"]`) and title (`h2 span`/`h2`);
/// dedupe by the query-stripped `/dp/` URL.
const AMAZON_SCRAPE_JS: &str = r#"JSON.stringify((function(){var seen={};var out=[];document.querySelectorAll('[data-component-type="s-search-result"]').forEach(function(el){var a=el.querySelector('a[href*="/dp/"]');var h=el.querySelector('h2 span, h2');if(a&&h){var u=a.href.split('?')[0];var t=(h.innerText||'').trim();if(t&&!seen[u]){seen[u]=1;out.push({url:u,title:t,content:''});}}});return out;})())"#;

/// The extractor JS for a browser-driven engine. Each has dedicated selectors
/// tuned against its live SERP — this is the single place to fix when a DOM
/// drifts. GitHub is *not* browser-driven (it uses the REST Search API), so it
/// never reaches here.
fn scrape_js(engine: SearchEngine) -> &'static str {
    match engine {
        SearchEngine::Google => GOOGLE_SCRAPE_JS,
        SearchEngine::Bing => BING_SCRAPE_JS,
        SearchEngine::DuckDuckGo => DDG_SCRAPE_JS,
        SearchEngine::Wikipedia => WIKIPEDIA_SCRAPE_JS,
        SearchEngine::Youtube => YOUTUBE_SCRAPE_JS,
        SearchEngine::Reddit => REDDIT_SCRAPE_JS,
        SearchEngine::Amazon => AMAZON_SCRAPE_JS,
        SearchEngine::Github => unreachable!("github uses the REST Search API, not the browser"),
    }
}

/// The camofox `navigate` request body for a browser-driven engine + query.
/// Google uses the browser's built-in `@google_search` macro (it handles the
/// consent/redirect dance). Bing/DuckDuckGo have no working macro in
/// camofox-browser, so we navigate their search URL directly — the macro is
/// only URL shorthand anyway. `query` is form-url-encoded into the `q`
/// parameter. GitHub is handled via the REST Search API and never reaches here.
fn navigate_body(engine: SearchEngine, query: &str) -> serde_json::Value {
    let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    match engine {
        SearchEngine::Google => {
            json!({ "userId": USER_ID, "macro": "@google_search", "query": query })
        }
        SearchEngine::Bing => {
            json!({ "userId": USER_ID, "url": format!("https://www.bing.com/search?q={q}") })
        }
        SearchEngine::DuckDuckGo => {
            json!({ "userId": USER_ID, "url": format!("https://duckduckgo.com/?q={q}") })
        }
        SearchEngine::Wikipedia => {
            // `fulltext=1` forces the search-results page; without it Wikipedia
            // redirects an exact title match straight to the article.
            json!({ "userId": USER_ID, "url": format!("https://en.wikipedia.org/wiki/Special:Search?search={q}&fulltext=1") })
        }
        SearchEngine::Youtube => {
            json!({ "userId": USER_ID, "url": format!("https://www.youtube.com/results?search_query={q}") })
        }
        SearchEngine::Reddit => {
            json!({ "userId": USER_ID, "url": format!("https://www.reddit.com/search/?q={q}") })
        }
        SearchEngine::Amazon => {
            json!({ "userId": USER_ID, "url": format!("https://www.amazon.com/s?k={q}") })
        }
        SearchEngine::Github => {
            unreachable!("github uses the REST Search API, not the browser")
        }
    }
}

/// One scraped SERP row, as emitted by the per-engine extractors ([`scrape_js`]).
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

/// GitHub REST Search API (`/search/repositories`) response — only the fields
/// we map into a result row.
#[derive(Deserialize)]
struct GithubSearchResponse {
    #[serde(default)]
    items: Vec<GithubRepo>,
}

#[derive(Deserialize)]
struct GithubRepo {
    html_url: String,
    full_name: String,
    #[serde(default)]
    description: Option<String>,
}

/// Search client backed by a camofox-browser REST endpoint. Returns the same
/// [`SearxngResponse`] shape as [`crate::client::SearxngClient`] so callers can
/// treat the two interchangeably.
pub struct CamofoxSearchClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Optional GitHub PAT for the `github` engine, which uses the GitHub REST
    /// Search API (not the browser) because GitHub web search rate-limits
    /// unauthenticated scraping. `None` falls back to the lower unauth quota.
    github_token: Option<String>,
    /// Base URL of the GitHub REST API for the `github` engine. Always
    /// `https://api.github.com` in production; overridden in tests to point at
    /// a mock server.
    github_api_base: String,
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
    /// `github_token` authenticates the `github` engine's Search-API calls.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        github_token: Option<String>,
        timeout: Duration,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base_url,
            api_key,
            github_token,
            github_api_base: "https://api.github.com".to_string(),
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

    /// Run the requested engines via Camofox and map the merged rows into a
    /// [`SearxngResponse`]. Typed [`SearchError`]s match the SearXNG client so
    /// the route layer's existing error mapping applies unchanged.
    ///
    /// Serializes on the warm-tab mutex (see [`Self::tab`]) so only one search
    /// touches the browser at a time, reusing the one long-lived tab. The
    /// engines in `params.camofox_engines` are run sequentially on that tab
    /// (the single-tab design dodges camofox's teardown race, so fan-out is
    /// serial — N engines ≈ N× latency). A stale tab is recreated and the
    /// engine retried once. An engine that still fails is skipped; results from
    /// the engines that succeeded are merged and returned. Only when *every*
    /// engine fails is the last error surfaced.
    pub async fn fetch(&self, params: &SearxngParams) -> Result<SearxngResponse, SearchError> {
        let mut tab = self.tab.lock().await;
        let mut all: Vec<SearxngResult> = Vec::new();
        let mut last_err: Option<SearchError> = None;
        let mut any_ok = false;

        for &engine in &params.camofox_engines {
            // GitHub uses the REST Search API, not the browser — no tab, no
            // stale-tab retry. Every other engine drives the warm camofox tab.
            let outcome = if matches!(engine, SearchEngine::Github) {
                self.github_search(&params.q).await
            } else {
                match self.attempt(&mut tab, engine, params).await {
                    Ok(rows) => Ok(rows),
                    Err(e) if is_stale_tab(&e) => {
                        // Warm tab/context died (idle eviction or camofox
                        // restart). Drop the dead id, let any in-flight relaunch
                        // settle, then recreate and retry this engine once.
                        *tab = None;
                        tokio::time::sleep(RETRY_BACKOFF).await;
                        self.attempt(&mut tab, engine, params).await
                    }
                    Err(e) => Err(e),
                }
            };
            match outcome {
                Ok(rows) => {
                    any_ok = true;
                    all.extend(rows);
                }
                Err(e) => last_err = Some(e),
            }
        }

        if !any_ok {
            return Err(last_err.unwrap_or(SearchError::Timeout));
        }
        Ok(merge_results(params.q.clone(), all))
    }

    /// Ensure a warm tab exists, then run one engine's search against it. Caller
    /// holds the tab mutex, so this is the single in-flight search.
    async fn attempt(
        &self,
        tab: &mut Option<String>,
        engine: SearchEngine,
        params: &SearxngParams,
    ) -> Result<Vec<SearxngResult>, SearchError> {
        let tab_id = self.ensure_tab(tab).await?;
        self.run_search(&tab_id, engine, params).await
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
        engine: SearchEngine,
        params: &SearxngParams,
    ) -> Result<Vec<SearxngResult>, SearchError> {
        let nav = self
            .post(
                &format!("/tabs/{tab_id}/navigate"),
                navigate_body(engine, &params.q),
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
                json!({ "userId": USER_ID, "expression": scrape_js(engine) }),
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
        let label = engine.label();
        let results = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearxngResult {
                url: Some(r.url),
                title: Some(r.title),
                engine: Some(label.to_string()),
                content: (!r.content.is_empty()).then_some(r.content),
                // Synthesize a descending score from SERP position so the
                // existing score-sort in transform.rs preserves engine order.
                score: Some((n - i) as f64),
                engines: vec![label.to_string()],
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

        Ok(results)
    }

    /// Search GitHub repositories via the REST Search API. Used instead of the
    /// browser because GitHub's web search rate-limits unauthenticated scraping
    /// almost immediately; the API gives clean JSON and a token lifts the quota.
    /// GitHub requires a `User-Agent`; the token (when set) is sent as a bearer.
    async fn github_search(&self, query: &str) -> Result<Vec<SearxngResult>, SearchError> {
        let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        let url = format!("{}/search/repositories?q={q}&per_page=10", self.github_api_base);
        let mut req = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "crw-search");
        if let Some(token) = &self.github_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e: reqwest::Error| {
            if e.is_timeout() {
                SearchError::Timeout
            } else {
                SearchError::Transport(e.without_url().to_string())
            }
        })?;
        if !resp.status().is_success() {
            return Err(SearchError::Upstream {
                status: resp.status().as_u16(),
                body: "github: search failed".to_string(),
            });
        }
        let data = resp
            .json::<GithubSearchResponse>()
            .await
            .map_err(|e| SearchError::InvalidResponse(format!("github: bad search response: {e}")))?;

        let n = data.items.len();
        Ok(data
            .items
            .into_iter()
            .enumerate()
            .map(|(i, r)| SearxngResult {
                url: Some(r.html_url),
                title: Some(r.full_name),
                engine: Some("github".to_string()),
                content: r.description.filter(|d| !d.is_empty()),
                score: Some((n - i) as f64),
                engines: vec!["github".to_string()],
                positions: vec![(i + 1) as u32],
                category: Some("general".to_string()),
                template: None,
                published_date: None,
                img_src: None,
                thumbnail_src: None,
                img_format: None,
                resolution: None,
            })
            .collect())
    }
}

/// Merge per-engine result rows into one response, deduped by URL. A URL seen
/// by multiple engines accumulates their `engines`/`positions` and sums their
/// position-scores, so cross-engine agreement ranks higher. First-appearance
/// order is preserved; downstream `rerank` does the final ordering.
fn merge_results(query: String, rows: Vec<SearxngResult>) -> SearxngResponse {
    use std::collections::HashMap;
    let mut order: Vec<String> = Vec::new();
    let mut by_url: HashMap<String, SearxngResult> = HashMap::new();
    for r in rows {
        let key = r.url.clone().unwrap_or_default();
        if let Some(existing) = by_url.get_mut(&key) {
            existing.engines.extend(r.engines);
            existing.positions.extend(r.positions);
            existing.score = Some(existing.score.unwrap_or(0.0) + r.score.unwrap_or(0.0));
        } else {
            order.push(key.clone());
            by_url.insert(key, r);
        }
    }
    let results: Vec<SearxngResult> = order
        .into_iter()
        .filter_map(|k| by_url.remove(&k))
        .collect();
    SearxngResponse {
        query,
        number_of_results: results.len() as u64,
        results,
        ..Default::default()
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

#[cfg(test)]
mod extractor_tests {
    use super::*;
    use crw_core::types::SearchEngine;

    #[test]
    fn browser_engines_have_dedicated_extractors() {
        // GitHub is excluded: it uses the REST Search API, not the browser.
        assert!(scrape_js(SearchEngine::Google).contains("div.g"));
        assert!(scrape_js(SearchEngine::Bing).contains("li.b_algo"));
        assert!(scrape_js(SearchEngine::DuckDuckGo).contains("article"));
        assert!(scrape_js(SearchEngine::Wikipedia).contains("mw-search-result-heading"));
        assert!(scrape_js(SearchEngine::Youtube).contains("ytd-video-renderer"));
        assert!(scrape_js(SearchEngine::Reddit).contains("/comments/"));
        assert!(scrape_js(SearchEngine::Amazon).contains("s-search-result"));
    }

    #[test]
    fn google_navigates_by_macro_others_by_url() {
        let g = navigate_body(SearchEngine::Google, "rust lang");
        assert_eq!(g["macro"], "@google_search");
        assert_eq!(g["query"], "rust lang");

        // Non-macro browser engines navigate a search URL, query url-encoded.
        let b = navigate_body(SearchEngine::Bing, "rust lang");
        assert_eq!(b["url"], "https://www.bing.com/search?q=rust+lang");
        let d = navigate_body(SearchEngine::DuckDuckGo, "rust lang");
        assert_eq!(d["url"], "https://duckduckgo.com/?q=rust+lang");
        let w = navigate_body(SearchEngine::Wikipedia, "rust lang");
        assert_eq!(
            w["url"],
            "https://en.wikipedia.org/wiki/Special:Search?search=rust+lang&fulltext=1"
        );
        let y = navigate_body(SearchEngine::Youtube, "rust lang");
        assert_eq!(y["url"], "https://www.youtube.com/results?search_query=rust+lang");
        let rd = navigate_body(SearchEngine::Reddit, "rust lang");
        assert_eq!(rd["url"], "https://www.reddit.com/search/?q=rust+lang");
        let am = navigate_body(SearchEngine::Amazon, "rust lang");
        assert_eq!(am["url"], "https://www.amazon.com/s?k=rust+lang");
    }
}

#[cfg(test)]
mod github_api_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `github_search` hits the REST Search API and maps `items[]` into result
    /// rows: `html_url` → url, `full_name` → title, `description` → content,
    /// with the engine tagged `github` and descending position scores.
    #[tokio::test]
    async fn github_search_maps_items_to_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/repositories"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    { "html_url": "https://github.com/a/one", "full_name": "a/one", "description": "first" },
                    { "html_url": "https://github.com/b/two", "full_name": "b/two", "description": null },
                ]
            })))
            .mount(&server)
            .await;

        let mut client =
            CamofoxSearchClient::new("http://unused", None, Some("tok".into()), Duration::from_secs(5));
        client.github_api_base = server.uri();

        let rows = client.github_search("rust").await.expect("github search ok");
        assert_eq!(rows.len(), 2);

        let first = &rows[0];
        assert_eq!(first.url.as_deref(), Some("https://github.com/a/one"));
        assert_eq!(first.title.as_deref(), Some("a/one"));
        assert_eq!(first.content.as_deref(), Some("first"));
        assert_eq!(first.engine.as_deref(), Some("github"));
        // A null description maps to no content.
        assert_eq!(rows[1].content, None);
        // Descending score by position so the merge ranks earlier hits higher.
        assert!(rows[0].score.unwrap() > rows[1].score.unwrap());
    }

    /// A non-2xx GitHub response surfaces as an `Upstream` error (not a panic or
    /// silent empty), so the fetch loop records it and skips the engine.
    #[tokio::test]
    async fn github_search_maps_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/repositories"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let mut client =
            CamofoxSearchClient::new("http://unused", None, None, Duration::from_secs(5));
        client.github_api_base = server.uri();

        let err = client.github_search("rust").await.unwrap_err();
        assert!(matches!(err, SearchError::Upstream { status: 403, .. }));
    }

    /// Partial-failure skip: a multi-engine fetch where one engine fails must
    /// still return the others' rows (the failed engine is skipped, not fatal).
    /// Here Google (browser) succeeds and GitHub (API) 500s; the response holds
    /// only Google's row.
    #[tokio::test]
    async fn fetch_skips_failed_engine_and_returns_partial() {
        let server = MockServer::start().await;
        // Camofox browser flow for the Google engine → one row.
        let rows = serde_json::to_string(&json!([
            { "url": "https://rust-lang.org", "title": "Rust", "content": "" },
        ]))
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tabId": "t1" })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/navigate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/wait"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tabs/t1/evaluate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": rows })))
            .mount(&server)
            .await;
        // GitHub API fails.
        Mock::given(method("GET"))
            .and(path("/search/repositories"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let mut client =
            CamofoxSearchClient::new(server.uri(), None, None, Duration::from_secs(5));
        client.github_api_base = server.uri();

        let params = SearxngParams {
            q: "rust".to_string(),
            camofox_engines: vec![SearchEngine::Google, SearchEngine::Github],
            ..Default::default()
        };
        let resp = client.fetch(&params).await.expect("partial success, not error");
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].url.as_deref(), Some("https://rust-lang.org"));
        assert_eq!(resp.results[0].engine.as_deref(), Some("google"));
    }

    /// When *every* engine fails, the fetch surfaces an error (not an empty Ok).
    #[tokio::test]
    async fn fetch_errors_when_all_engines_fail() {
        let server = MockServer::start().await;
        // Tab creation fails → the Google engine errors; no other engine.
        Mock::given(method("POST"))
            .and(path("/tabs"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = CamofoxSearchClient::new(server.uri(), None, None, Duration::from_secs(5));
        let params = SearxngParams {
            q: "rust".to_string(),
            camofox_engines: vec![SearchEngine::Google],
            ..Default::default()
        };
        assert!(client.fetch(&params).await.is_err());
    }
}
