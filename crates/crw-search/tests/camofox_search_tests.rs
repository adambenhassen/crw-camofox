//! Behavioural tests for the Camofox-backed Google search client. A wiremock
//! server emulates the camofox-browser REST flow (create tab → navigate with
//! the `@google_search` macro → wait → evaluate the result-scrape JS → close)
//! and we assert the rows map into the existing `SearxngResponse` shape so the
//! downstream transform/rerank pipeline is reused unchanged.

use std::time::Duration;

use crw_core::types::SearchEngine;
use crw_search::SearxngParams;
use crw_search::camofox_search::CamofoxSearchClient;
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn params(q: &str) -> SearxngParams {
    params_with_engines(q, vec![SearchEngine::Google])
}

fn params_with_engines(q: &str, engines: Vec<SearchEngine>) -> SearxngParams {
    SearxngParams {
        q: q.to_string(),
        categories: None,
        language: None,
        time_range: None,
        engines: None,
        pageno: None,
        safesearch: None,
        camofox_engines: engines,
    }
}

/// Stand up a camofox-browser mock that returns two Google rows.
async fn mock_with_rows(rows: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    // The scrape JS returns a JSON *string* (the client `JSON.parse`s it).
    let result_string = serde_json::to_string(&rows).unwrap();

    // The real camofox-browser requires both userId and sessionKey — guard it.
    Mock::given(method("POST"))
        .and(path("/tabs"))
        .and(body_partial_json(json!({ "sessionKey": "search" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "tabId": "tab-1", "sessionKey": "search"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-1/navigate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-1/wait"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-1/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": result_string, "resultType": "string", "truncated": false
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/tabs/tab-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn fetch_maps_scraped_rows_to_searxng_response() {
    let server = mock_with_rows(json!([
        { "url": "https://a.example", "title": "Result A", "content": "snippet a" },
        { "url": "https://b.example", "title": "Result B", "content": "snippet b" },
    ]))
    .await;

    let client = CamofoxSearchClient::new(server.uri(), None, None, Duration::from_secs(10));
    let resp = client
        .fetch(&params("rust async"))
        .await
        .expect("camofox search should succeed against the mock");

    assert_eq!(resp.query, "rust async");
    assert_eq!(resp.results.len(), 2);

    let first = &resp.results[0];
    assert_eq!(first.url.as_deref(), Some("https://a.example"));
    assert_eq!(first.title.as_deref(), Some("Result A"));
    assert_eq!(first.content.as_deref(), Some("snippet a"));
    assert_eq!(first.engine.as_deref(), Some("google"));

    // Rank must descend with SERP position so the existing score-sort keeps order.
    let s0 = resp.results[0].score.unwrap_or(0.0);
    let s1 = resp.results[1].score.unwrap_or(0.0);
    assert!(s0 > s1, "expected descending scores by position, got {s0} !> {s1}");
}

/// Two engines run over the one warm tab; identical URLs returned by both must
/// dedupe to a single row that accumulates BOTH engine labels and positions and
/// sums their scores (cross-engine agreement ranks higher).
#[tokio::test]
async fn two_engines_merge_and_dedupe_by_url() {
    let server = mock_with_rows(json!([
        { "url": "https://a.example", "title": "Result A", "content": "snippet a" },
        { "url": "https://b.example", "title": "Result B", "content": "snippet b" },
    ]))
    .await;

    let client = CamofoxSearchClient::new(server.uri(), None, None, Duration::from_secs(10));
    let resp = client
        .fetch(&params_with_engines(
            "rust async",
            vec![SearchEngine::Google, SearchEngine::Bing],
        ))
        .await
        .expect("multi-engine camofox search should succeed");

    // Same two URLs from both engines → 2 unique rows after dedupe.
    assert_eq!(resp.results.len(), 2);
    let first = &resp.results[0];
    assert_eq!(first.url.as_deref(), Some("https://a.example"));
    // Both engines recorded, positions accumulated, scores summed.
    assert_eq!(first.engines, vec!["google".to_string(), "bing".to_string()]);
    assert_eq!(first.positions.len(), 2);
    assert_eq!(first.score, Some(4.0)); // (2 from google) + (2 from bing)
}

#[tokio::test]
async fn fetch_tolerates_empty_results() {
    let server = mock_with_rows(json!([])).await;
    let client = CamofoxSearchClient::new(server.uri(), None, None, Duration::from_secs(10));
    let resp = client.fetch(&params("nothing here")).await.unwrap();
    assert_eq!(resp.results.len(), 0);
}

/// Two back-to-back searches must drive ONE warm tab — not create+delete a tab
/// per query — so the camofox context never hits zero tabs and never trips the
/// upstream eager-teardown race. We assert exactly one `POST /tabs` across two
/// `fetch` calls (and that we never DELETE the warm tab).
#[tokio::test]
async fn reuses_one_warm_tab_across_sequential_searches() {
    let server = MockServer::start().await;
    let rows = serde_json::to_string(&json!([
        { "url": "https://a.example", "title": "A", "content": "" },
    ]))
    .unwrap();

    // Exactly one tab is ever created, regardless of how many searches run.
    Mock::given(method("POST"))
        .and(path("/tabs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tabId": "tab-1" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-1/navigate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-1/wait"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-1/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": rows })))
        .mount(&server)
        .await;
    // The warm tab must never be deleted — fail loudly if it is.
    Mock::given(method("DELETE"))
        .and(path("/tabs/tab-1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let client = CamofoxSearchClient::new(server.uri(), None, None, Duration::from_secs(10));
    client.fetch(&params("first")).await.expect("first search ok");
    client.fetch(&params("second")).await.expect("second search ok");
    // wiremock verifies `.expect(..)` counts on drop.
}

/// If the warm tab went stale (context idle-evicted or camofox restarted), the
/// first navigate fails; the client must drop the dead tab, create a fresh one,
/// and retry the search — recovering transparently.
#[tokio::test]
async fn recreates_tab_and_retries_after_stale_failure() {
    let server = MockServer::start().await;
    let rows = serde_json::to_string(&json!([
        { "url": "https://a.example", "title": "A", "content": "" },
    ]))
    .unwrap();

    // First create hands out the (soon-stale) tab-1; the next create hands out
    // tab-2. up_to_n_times + priority makes tab-1 serve once, then tab-2.
    Mock::given(method("POST"))
        .and(path("/tabs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tabId": "tab-1" })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tabId": "tab-2" })))
        .with_priority(2)
        .mount(&server)
        .await;
    // tab-1 is dead: navigate returns 500 (the upstream "window is null" shape).
    Mock::given(method("POST"))
        .and(path("/tabs/tab-1/navigate"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    // tab-2 is healthy.
    Mock::given(method("POST"))
        .and(path("/tabs/tab-2/navigate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-2/wait"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-2/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": rows })))
        .mount(&server)
        .await;

    let client = CamofoxSearchClient::new(server.uri(), None, None, Duration::from_secs(10));
    let resp = client
        .fetch(&params("recover"))
        .await
        .expect("search should recover by recreating the tab");
    assert_eq!(resp.results.len(), 1);
}
