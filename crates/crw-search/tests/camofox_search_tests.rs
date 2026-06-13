//! Behavioural tests for the Camofox-backed Google search client. A wiremock
//! server emulates the camofox-browser REST flow (create tab → navigate with
//! the `@google_search` macro → wait → evaluate the result-scrape JS → close)
//! and we assert the rows map into the existing `SearxngResponse` shape so the
//! downstream transform/rerank pipeline is reused unchanged.

use std::time::Duration;

use crw_search::SearxngParams;
use crw_search::camofox_search::CamofoxSearchClient;
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn params(q: &str) -> SearxngParams {
    SearxngParams {
        q: q.to_string(),
        categories: None,
        language: None,
        time_range: None,
        engines: None,
        pageno: None,
        safesearch: None,
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

    let client = CamofoxSearchClient::new(server.uri(), None, Duration::from_secs(10));
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

#[tokio::test]
async fn fetch_tolerates_empty_results() {
    let server = mock_with_rows(json!([])).await;
    let client = CamofoxSearchClient::new(server.uri(), None, Duration::from_secs(10));
    let resp = client.fetch(&params("nothing here")).await.unwrap();
    assert_eq!(resp.results.len(), 0);
}
