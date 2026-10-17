//! DELETE on a crawl/batch job must leave it in a terminal `Cancelled` state:
//! polls report it, re-cancel is rejected, TTL cleanup can evict it, and a
//! scrape that finishes after the cancel cannot flip it back to `Completed`.

use axum::extract::{Path, State};
use crw_core::config::AppConfig;
use crw_core::types::{CrawlStatus, ScrapeRequest};
use crw_server::state::AppState;
use uuid::Uuid;

/// Starts a one-URL batch job that stays `InProgress` until the returned
/// permits are dropped: the job task waits on the crawl semaphore, so the test
/// needs no network and has no timing race.
async fn pending_batch(state: &AppState) -> (Uuid, tokio::sync::OwnedSemaphorePermit) {
    let held = state
        .crawl_semaphore
        .clone()
        .acquire_many_owned(state.crawl_semaphore.available_permits() as u32)
        .await
        .unwrap();
    // `actions` is rejected per URL inside the scrape, so it fails fast offline.
    let template = ScrapeRequest {
        actions: Some(serde_json::json!([])),
        ..Default::default()
    };
    let id = state
        .start_batch_job(vec!["https://example.com/x".into()], template)
        .await;
    (id, held)
}

fn new_state() -> AppState {
    let config: AppConfig = toml::from_str("").unwrap();
    AppState::new(config).unwrap()
}

async fn status_of(state: &AppState, id: Uuid) -> CrawlStatus {
    state.crawl_jobs.read().await[&id].rx.borrow().status
}

#[tokio::test]
async fn v2_cancel_marks_the_job_cancelled_and_rejects_a_second_cancel() {
    let state = new_state();
    let (id, _held) = pending_batch(&state).await;
    assert_eq!(status_of(&state, id).await, CrawlStatus::InProgress);

    let Ok(axum::Json(body)) =
        crw_server::routes::v2::crawl::cancel_crawl(State(state.clone()), Path(id)).await
    else {
        panic!("first cancel should succeed");
    };
    assert_eq!(body["status"], "cancelled");
    assert_eq!(status_of(&state, id).await, CrawlStatus::Cancelled);

    let again = crw_server::routes::v2::batch::cancel_batch(State(state.clone()), Path(id)).await;
    assert!(
        again.is_err(),
        "re-cancel of a finished job must be rejected"
    );
}

#[tokio::test]
async fn v1_cancel_marks_the_job_cancelled_and_rejects_a_second_cancel() {
    let state = new_state();
    let (id, _held) = pending_batch(&state).await;

    let first = crw_server::routes::crawl::cancel_crawl(State(state.clone()), Path(id)).await;
    assert!(first.is_ok(), "first cancel should succeed");
    assert_eq!(status_of(&state, id).await, CrawlStatus::Cancelled);

    let again = crw_server::routes::crawl::cancel_crawl(State(state.clone()), Path(id)).await;
    assert!(
        again.is_err(),
        "re-cancel of a finished job must be rejected"
    );
}

#[tokio::test]
async fn a_finishing_batch_task_cannot_overwrite_cancelled() {
    let state = new_state();
    let (id, held) = pending_batch(&state).await;

    // Mark Cancelled without aborting the task, so the task still runs to the
    // end and tries to flip the job to Completed.
    state.crawl_jobs.read().await[&id]
        .tx
        .send_modify(|st| st.status = CrawlStatus::Cancelled);
    drop(held);

    let mut finished = false;
    for _ in 0..200 {
        tokio::task::yield_now().await;
        let jobs = state.crawl_jobs.read().await;
        let st = jobs[&id].rx.borrow().clone();
        if st.completed == st.total && st.total > 0 {
            finished = true;
            break;
        }
    }
    assert!(finished, "batch task did not finish");
    assert_eq!(status_of(&state, id).await, CrawlStatus::Cancelled);
}
