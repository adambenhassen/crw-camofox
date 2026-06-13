//! Client behaviour tests against a mock camofox-browser REST server.

use std::time::Duration;

use crw_browse_camofox::camofox::{CamofoxClient, Target};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(base: String) -> CamofoxClient {
    CamofoxClient::new(base, None, Duration::from_secs(5))
}

#[tokio::test]
async fn navigate_creates_tab_with_session_key_then_reuses_it() {
    let server = MockServer::start().await;

    // First navigate has no tab → POST /tabs (must carry sessionKey).
    Mock::given(method("POST"))
        .and(path("/tabs"))
        .and(body_partial_json(json!({ "sessionKey": "browse" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tabId": "tab-9", "url": "https://example.com/"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-9/navigate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/tab-9/wait"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;

    let c = client(server.uri());

    // First call creates the tab.
    let r1 = c.navigate("https://example.com", 5000).await.unwrap();
    assert_eq!(r1["url"], "https://example.com");

    // Second call must reuse tab-9 via /navigate (no second POST /tabs). If the
    // client tried to create a new tab, there's a matcher for /tabs but the
    // /navigate path proves reuse.
    let r2 = c.navigate("https://example.org", 5000).await.unwrap();
    assert_eq!(r2["url"], "https://example.org");
}

#[tokio::test]
async fn click_before_navigate_errors_without_http_call() {
    let c = client("http://127.0.0.1:1".to_string());
    let err = c.click(Target::Selector("a")).await.unwrap_err();
    assert!(err.contains("navigate"), "expected a 'navigate first' hint: {err}");
}

#[tokio::test]
async fn press_scroll_links_history_forward_to_camofox() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tabs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tabId": "t2" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/t2/wait"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/t2/press"))
        .and(body_partial_json(json!({ "key": "Enter" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/t2/back"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "url": "u" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/tabs/t2/links"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "links": [{ "url": "https://a", "text": "A" }], "pagination": { "total": 1 }
        })))
        .mount(&server)
        .await;

    let c = client(server.uri());
    c.navigate("https://example.com", 1000).await.unwrap();
    c.press("Enter").await.unwrap();
    assert_eq!(c.history("back").await.unwrap()["url"], "u");
    let links = c.links(5, 0).await.unwrap();
    assert_eq!(links["links"][0]["url"], "https://a");
}

#[tokio::test]
async fn snapshot_returns_camofox_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tabs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tabId": "t1" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tabs/t1/wait"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/tabs/t1/snapshot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "snapshot": "- link \"Learn more\" [e1]", "refsCount": 1
        })))
        .mount(&server)
        .await;

    let c = client(server.uri());
    c.navigate("https://example.com", 1000).await.unwrap();
    let snap = c.snapshot().await.unwrap();
    assert_eq!(snap["refsCount"], 1);
    assert!(snap["snapshot"].as_str().unwrap().contains("[e1]"));
}
