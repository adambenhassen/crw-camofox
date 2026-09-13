//! A batch URL whose scrape errors must come back as a blocked placeholder that
//! names the reason, not vanish from `data` while `completed` still advances.

use crw_core::config::AppConfig;
use crw_core::types::{CrawlStatus, ScrapeRequest};
use crw_server::state::AppState;

#[tokio::test]
async fn start_batch_job_records_scrape_errors_as_blocked_documents() {
    let config: AppConfig = toml::from_str("").unwrap();
    let state = AppState::new(config).unwrap();
    // Rejected by `scrape_url` before any fetch, so the test needs no network.
    let template = ScrapeRequest {
        actions: Some(serde_json::json!([])),
        ..Default::default()
    };
    let url = "https://example.com/error";
    let id = state.start_batch_job(vec![url.to_string()], template).await;

    let mut settled = None;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        let jobs = state.crawl_jobs.read().await;
        let job = jobs.get(&id).unwrap().rx.borrow().clone();
        if job.status != CrawlStatus::InProgress {
            settled = Some(job);
            break;
        }
    }
    let job = settled.expect("batch job did not settle");
    assert_eq!(job.status, CrawlStatus::Completed);
    assert_eq!(job.completed, 1);
    assert_eq!(job.blocked, 1);
    assert_eq!(job.data.len(), 1);
    assert_eq!(job.data[0].metadata.source_url, url);
    let block = job.data[0]
        .block
        .as_ref()
        .expect("failed URL carries a block");
    assert_eq!(block.vendor, crw_core::types::HTTP_ERROR_VENDOR);
    // Substring, not the whole sentence: the wording lives in `crw-crawl` and
    // rewording a customer-facing message must not break a `crw-server` test.
    assert!(
        block.reason.contains("actions"),
        "reason should name the rejected parameter, got: {}",
        block.reason
    );
}
