//! `POST /v2/crawl`, `GET/DELETE /v2/crawl/{id}`, `GET /v2/crawl/active`,
//! `GET /v2/crawl/{id}/errors`. Reuses the existing in-memory crawl-job engine
//! (`AppState::start_crawl_job` + `crawl_jobs`); only the wire shapes differ.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

use crw_core::error::CrwError;
use crw_core::types::{CrawlRequest, CrawlStatus, OutputFormat, RequestedRenderer};

use super::adapters::{DEFAULT_PAGE_LIMIT, V2CrawlStatus, build_crawl_status};
use super::formats::{FormatSpec, decompose};
use crate::error::AppError;
use crate::state::{AppState, validate_crawl_renderer};

/// Derive the public scheme+host for `next`/`url` from the inbound request.
/// Matches Firecrawl (uses the request Host). Behind the SaaS proxy the path is
/// rewritten there; the SDK overrides the host anyway, keeping only path+query.
pub(crate) fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V2CrawlRequest {
    pub url: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub max_discovery_depth: Option<u32>,
    /// Nested per-page scrape options. We thread `formats`/`onlyMainContent`/
    /// `waitFor` through to the engine (not just tolerate them).
    #[serde(default)]
    pub scrape_options: Option<Value>,
    #[serde(default)]
    pub renderer: Option<RequestedRenderer>,
    #[serde(default)]
    pub country: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct V2CrawlStartResponse {
    pub success: bool,
    pub id: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub skip: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Internal projection of a v2 `scrapeOptions` object.
pub(crate) struct ScrapeOpts {
    pub formats: Vec<OutputFormat>,
    pub headers: HashMap<String, String>,
    pub json_schema: Option<Value>,
    pub only_main_content: bool,
    pub wait_for: Option<u64>,
}

/// Pull the internal scrape projection out of a v2 `scrapeOptions` object.
pub(crate) fn scrape_opts_to_internal(opts: &Option<Value>) -> Result<ScrapeOpts, CrwError> {
    let mut out = ScrapeOpts {
        formats: vec![OutputFormat::Markdown],
        headers: HashMap::new(),
        json_schema: None,
        only_main_content: true,
        wait_for: None,
    };
    if let Some(Value::Object(m)) = opts {
        if let Some(f) = m.get("formats") {
            let specs: Vec<FormatSpec> = serde_json::from_value(f.clone()).map_err(|e| {
                CrwError::InvalidRequest(format!("invalid scrapeOptions.formats: {e}"))
            })?;
            let d = decompose(&specs).map_err(CrwError::InvalidRequest)?;
            out.formats = d.formats;
            out.json_schema = d.json_schema;
        }
        // Firecrawl carries per-page request headers here, and `/v2/scrape`
        // already honours its own `headers`. Dropping them on the crawl path
        // would be the same silent-ignore failure as #346, so a non-object (or
        // a non-string value) is an error rather than a quiet no-op.
        if let Some(h) = m.get("headers")
            && !h.is_null()
        {
            out.headers = serde_json::from_value(h.clone()).map_err(|e| {
                CrwError::InvalidRequest(format!(
                    "scrapeOptions.headers must be an object of strings: {e}"
                ))
            })?;
        }
        if let Some(b) = m.get("onlyMainContent").and_then(Value::as_bool) {
            out.only_main_content = b;
        }
        if let Some(w) = m.get("waitFor").and_then(Value::as_u64) {
            out.wait_for = Some(w);
        }
    }
    Ok(out)
}

pub async fn start_crawl(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<V2CrawlRequest>, JsonRejection>,
) -> Result<Json<V2CrawlStartResponse>, AppError> {
    let Json(v2) = body.map_err(AppError::from)?;
    let parsed_url = url::Url::parse(&v2.url)
        .map_err(|e| CrwError::InvalidRequest(format!("Invalid URL: {e}")))?;
    crw_core::url_safety::validate_safe_url_resolved(&parsed_url)
        .await
        .map_err(CrwError::InvalidRequest)?;

    let opts = scrape_opts_to_internal(&v2.scrape_options)?;
    let req = CrawlRequest {
        headers: opts.headers,
        url: v2.url.clone(),
        max_depth: v2.max_discovery_depth,
        max_pages: v2.limit,
        formats: opts.formats,
        only_main_content: opts.only_main_content,
        json_schema: opts.json_schema,
        render_js: None,
        wait_for: opts.wait_for,
        renderer: v2.renderer,
        country: v2.country,
    };
    validate_crawl_renderer(&req, &state)?;

    let id = state.start_crawl_job(req).await;
    let base = base_url(&headers);
    Ok(Json(V2CrawlStartResponse {
        success: true,
        id: id.to_string(),
        url: format!("{base}/v2/crawl/{id}"),
    }))
}

pub async fn get_crawl(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(page): Query<PageQuery>,
) -> Result<Json<V2CrawlStatus>, AppError> {
    let (snapshot, created_at) = {
        let jobs = state.crawl_jobs.read().await;
        let job = jobs
            .get(&id)
            .ok_or_else(|| CrwError::NotFound(format!("Crawl job {id} not found")))?;
        (job.rx.borrow().clone(), job.created_at)
    };
    let skip = page.skip.unwrap_or(0);
    let limit = page.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    let base = base_url(&headers);
    let status = build_crawl_status(
        &snapshot,
        created_at,
        state.config.crawler.job_ttl_secs,
        skip,
        limit,
        &base,
        "/v2/crawl",
        id,
        "basic",
    );
    Ok(Json(status))
}

pub async fn cancel_crawl(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let mut jobs = state.crawl_jobs.write().await;
    let job = jobs
        .get_mut(&id)
        .ok_or_else(|| CrwError::NotFound(format!("Crawl job {id} not found")))?;
    let status = job.rx.borrow().status;
    if matches!(status, CrawlStatus::Completed | CrawlStatus::Failed) {
        return Err(AppError(CrwError::InvalidRequest(
            "Crawl job already finished".into(),
        )));
    }
    if let Some(handle) = job.abort_handle.take() {
        handle.abort();
    }
    Ok(Json(serde_json::json!({
        "success": true,
        "status": "cancelled",
        "message": format!("Crawl job {id} cancelled"),
    })))
}

/// `GET /v2/crawl/active` (Tier-3) — list still-running job ids.
pub async fn active(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let jobs = state.crawl_jobs.read().await;
    let ids: Vec<String> = jobs
        .iter()
        .filter(|(_, j)| matches!(j.rx.borrow().status, CrawlStatus::InProgress))
        .map(|(id, _)| id.to_string())
        .collect();
    Ok(Json(serde_json::json!({ "success": true, "crawls": ids })))
}

/// `GET /v2/crawl/{id}/errors` (Tier-3).
pub async fn get_errors(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let jobs = state.crawl_jobs.read().await;
    let job = jobs
        .get(&id)
        .ok_or_else(|| CrwError::NotFound(format!("Crawl job {id} not found")))?;
    // Job-level failure first (the whole job died), then the per-URL ones. A URL
    // the engine could not turn into a document is retained as a placeholder
    // carrying `block` and no body, and `V2Document` has no field to show the
    // block, so without this it would reach the caller as an empty document
    // with nothing to explain it while this route, the one documented to carry
    // these, said there were no errors at all. An origin error page that was
    // kept readable is a delivered document, not an error: listing it here too
    // would make a caller retry a URL it already holds.
    //
    // Each entry gets its own id, the way Firecrawl keys errors per scrape
    // rather than per job, so a caller keying them into a map keeps them all.
    // The id is the job id plus the document's position, so it is stable
    // across polls and a caller deduplicating by id sees each failure once.
    let guard = job.rx.borrow();
    let mut errors: Vec<Value> = guard
        .error
        .iter()
        .map(|e| serde_json::json!({ "id": id.to_string(), "error": e }))
        .collect();
    errors.extend(
        guard
            .data
            .iter()
            .enumerate()
            .filter(|(_, d)| !d.has_body())
            .filter_map(|(index, d)| {
                d.block.as_ref().map(|b| {
                    serde_json::json!({
                        "id": format!("{id}-{index}"),
                        "url": d.metadata.source_url,
                        "error": b.reason,
                    })
                })
            }),
    );
    drop(guard);
    Ok(Json(
        serde_json::json!({ "success": true, "errors": errors, "robotsBlocked": [] }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch URL the engine could not turn into a document is retained with a
    /// `block`, but `V2Document` has no field that can show it, so on this
    /// surface it is otherwise an empty document with nothing to explain it.
    /// This route is the one documented to carry those failures, so it has to.
    #[tokio::test]
    async fn get_errors_reports_the_per_url_failures_of_a_batch() {
        let config: crw_core::config::AppConfig = toml::from_str("").unwrap();
        let state = AppState::new(config).unwrap();
        // `actions` is rejected per URL inside the scrape, which is the shortest
        // deterministic way to make one URL fail without touching the network.
        let template = crw_core::types::ScrapeRequest {
            actions: Some(serde_json::json!([])),
            ..Default::default()
        };
        let url = "https://example.com/error";
        let id = state.start_batch_job(vec![url.to_string()], template).await;

        let mut settled = false;
        for _ in 0..100 {
            tokio::task::yield_now().await;
            let jobs = state.crawl_jobs.read().await;
            if jobs.get(&id).unwrap().rx.borrow().status != CrawlStatus::InProgress {
                settled = true;
                break;
            }
        }
        assert!(settled, "batch job did not settle");

        let Ok(Json(body)) = get_errors(State(state.clone()), Path(id)).await else {
            panic!("errors route should succeed for an existing job");
        };
        let errors = body["errors"].as_array().expect("errors array");
        assert_eq!(errors.len(), 1, "got: {body}");
        assert_eq!(errors[0]["url"], url, "got: {body}");
        assert!(
            errors[0]["error"]
                .as_str()
                .is_some_and(|e| e.contains("actions")),
            "got: {body}"
        );
    }

    /// Firecrawl bodies put per-page headers under `scrapeOptions`, and
    /// `/v2/scrape` already honours its own `headers`. A crawl that dropped
    /// them would be the same silent-ignore as #346.
    #[test]
    fn scrape_options_headers_reach_the_crawl_request() {
        let opts = Some(serde_json::json!({
            "headers": { "Authorization": "Bearer x", "X-Env": "staging" }
        }));
        let parsed = scrape_opts_to_internal(&opts).unwrap();
        assert_eq!(
            parsed.headers.get("Authorization"),
            Some(&"Bearer x".to_string())
        );
        assert_eq!(parsed.headers.get("X-Env"), Some(&"staging".to_string()));

        // Absent or null stays empty rather than erroring.
        assert!(scrape_opts_to_internal(&None).unwrap().headers.is_empty());
        assert!(
            scrape_opts_to_internal(&Some(serde_json::json!({ "headers": null })))
                .unwrap()
                .headers
                .is_empty()
        );
    }

    #[test]
    fn a_malformed_scrape_options_headers_is_rejected_not_ignored() {
        for bad in [
            serde_json::json!({ "headers": "Authorization: x" }),
            serde_json::json!({ "headers": { "X-Num": 1 } }),
            serde_json::json!({ "headers": ["a", "b"] }),
        ] {
            assert!(
                scrape_opts_to_internal(&Some(bad.clone())).is_err(),
                "should reject {bad}"
            );
        }
    }
}
