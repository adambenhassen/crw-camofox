//! Integration tests for `GET /v1/capabilities`.
//!
//! The point of these tests is ANTI-LIE: every advertised capability must be
//! backed by the build feature / config that actually enables it. A test here
//! fails if the endpoint claims something this instance cannot do.

use axum_test::TestServer;
use crw_core::config::AppConfig;
use crw_server::app::create_app;
use crw_server::state::AppState;
use serde_json::{Value, json};

fn state_from(toml_str: &str) -> AppState {
    let config: AppConfig = toml::from_str(toml_str).unwrap();
    AppState::new(config).expect("AppState::new failed")
}

fn app_from(toml_str: &str) -> TestServer {
    TestServer::new(create_app(state_from(toml_str)))
}

async fn caps(server: &TestServer) -> Value {
    let r = server.get("/v1/capabilities").await;
    r.assert_status_ok();
    r.json()
}

/// Search is served by the camofox tier, so it needs the endpoint configured.
const SEARCH_ON: &str = r#"
[search]
enabled = true

[renderer.camofox]
base_url = "http://127.0.0.1:9"
"#;

// ── Search: answer / summarizeResults are never advertised when off ──────────

#[tokio::test]
async fn search_off_reports_unsupported_and_no_answer() {
    // Default config has no search backend → state.search = None.
    let body = caps(&app_from("")).await;

    assert_eq!(body["search"]["supported"], json!(false));
    assert_eq!(
        body["search"]["answer"],
        json!(false),
        "answer must not be advertised when search is off"
    );
    assert_eq!(body["search"]["summarizeResults"], json!(false));
}

#[tokio::test]
async fn search_on_without_server_llm_key_reports_no_answer() {
    // Search configured but no [extraction.llm] → answering requires a
    // caller-supplied llmApiKey, so the instance cannot answer on its own.
    let body = caps(&app_from(SEARCH_ON)).await;

    assert_eq!(body["search"]["supported"], json!(true));
    assert_eq!(body["llm"]["serverKeyConfigured"], json!(false));
    assert_eq!(
        body["search"]["answer"],
        json!(false),
        "answer must not be advertised without a server LLM key"
    );
    assert_eq!(body["search"]["summarizeResults"], json!(false));
}

#[tokio::test]
async fn search_on_with_server_llm_key_reports_answer() {
    let body = caps(&app_from(&format!(
        r#"{SEARCH_ON}
[extraction.llm]
provider = "anthropic"
api_key = "sk-test"
model = "claude-sonnet-4-20250514"
"#
    )))
    .await;

    assert_eq!(body["search"]["supported"], json!(true));
    assert_eq!(body["llm"]["serverKeyConfigured"], json!(true));
    assert_eq!(body["search"]["answer"], json!(true));
    assert_eq!(body["search"]["summarizeResults"], json!(true));
}

#[tokio::test]
async fn llm_key_without_search_does_not_advertise_answer() {
    let body = caps(&app_from(
        r#"
[extraction.llm]
provider = "anthropic"
api_key = "sk-test"
model = "claude-sonnet-4-20250514"
"#,
    ))
    .await;

    assert_eq!(body["llm"]["serverKeyConfigured"], json!(true));
    assert_eq!(body["search"]["supported"], json!(false));
    assert_eq!(body["search"]["answer"], json!(false));
}

#[tokio::test]
async fn search_disabled_flag_overrides_configured_backend() {
    let body = caps(&app_from(
        r#"
[search]
enabled = false

[renderer.camofox]
base_url = "http://127.0.0.1:9"
"#,
    ))
    .await;

    assert_eq!(body["search"]["supported"], json!(false));
    assert_eq!(body["search"]["answer"], json!(false));
}

// ── Renderers: the tiers actually constructed, plus the effective mode ───────

#[tokio::test]
async fn renderers_report_the_constructed_tiers_and_mode() {
    let state = state_from(
        r#"
[renderer]
mode = "none"
render_js_default = false
"#,
    );
    let expected: Vec<String> = state
        .renderer
        .available_renderer_names()
        .into_iter()
        .map(String::from)
        .collect();
    let body = caps(&TestServer::new(create_app(state))).await;

    let available: Vec<String> =
        serde_json::from_value(body["renderers"]["available"].clone()).unwrap();
    assert_eq!(available, expected);
    assert!(
        !available
            .iter()
            .any(|n| n == "lightpanda" || n == "camofox"),
        "mode = none constructs no JS tier: {available:?}"
    );
    assert_eq!(body["renderers"]["mode"], json!("none"));
    assert_eq!(body["renderers"]["renderJsDefault"], json!(false));

    // renderJsDefault is omitted, not null, when unset.
    let unset = caps(&app_from("")).await;
    assert!(unset["renderers"].get("renderJsDefault").is_none());
    assert_eq!(unset["renderers"]["mode"], json!("auto"));
}

// ── Documents ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn documents_follow_pdf_feature_and_config() {
    let enabled = caps(&app_from("")).await;
    let parsers: Vec<String> =
        serde_json::from_value(enabled["documents"]["parsers"].clone()).unwrap();
    assert_eq!(
        !parsers.is_empty(),
        crw_extract::pdf::PDF_SUPPORTED,
        "a parser may only be advertised when the pdf feature is compiled in"
    );
    assert_eq!(
        enabled["documents"]["fileUpload"]["supported"],
        json!(crw_extract::pdf::PDF_SUPPORTED)
    );

    let disabled = caps(&app_from(
        r#"
[document]
enabled = false
"#,
    ))
    .await;
    assert_eq!(disabled["documents"]["parsers"], json!([]));
    assert_eq!(
        disabled["documents"]["fileUpload"]["supported"],
        json!(false)
    );
    assert_eq!(disabled["documents"]["fileUpload"]["types"], json!([]));
}

// ── Limits: echo the effective config, never a hardcoded literal ─────────────

#[tokio::test]
async fn limits_echo_a_non_default_config() {
    let body = caps(&app_from(
        r#"
[crawler]
max_batch_urls = 77

[search]
default_limit = 3
max_limit = 11

[document]
max_upload_bytes = 1048576
"#,
    ))
    .await;

    assert_eq!(body["limits"]["maxBatchUrls"], json!(77));
    assert_eq!(body["limits"]["searchDefaultLimit"], json!(3));
    assert_eq!(body["limits"]["searchMaxLimit"], json!(11));
    assert_eq!(body["limits"]["maxUploadBytes"], json!(1_048_576));
    assert_eq!(
        body["documents"]["fileUpload"]["maxBytes"],
        json!(1_048_576),
        "the advertised upload cap must be the enforced one"
    );
}

#[tokio::test]
async fn upload_cap_is_clamped_to_the_hard_ceiling() {
    // An operator cannot raise the cap past the in-memory ceiling, so the
    // advertised value must be the clamped (enforced) one, not the raw knob.
    let body = caps(&app_from(
        r#"
[document]
max_upload_bytes = 2000000000
"#,
    ))
    .await;

    assert_eq!(
        body["limits"]["maxUploadBytes"],
        json!(crw_server::routes::v2::parse::MAX_UPLOAD_BYTES)
    );
}

/// A well-formed multipart upload carrying `payload_len` bytes in a PDF field.
fn multipart_upload(payload_len: usize) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--BOUNDARY\r\n\
          Content-Disposition: form-data; name=\"file\"; filename=\"big.pdf\"\r\n\
          Content-Type: application/pdf\r\n\r\n",
    );
    body.extend_from_slice(&vec![b'a'; payload_len]);
    body.extend_from_slice(b"\r\n--BOUNDARY--\r\n");
    body
}

async fn upload_error(server: &TestServer, payload_len: usize) -> String {
    server
        .post("/v2/parse")
        .bytes(multipart_upload(payload_len).into())
        .add_header("content-type", "multipart/form-data; boundary=BOUNDARY")
        .expect_failure()
        .await
        .text()
}

#[tokio::test]
async fn advertised_upload_cap_is_the_one_enforced() {
    // The advertised cap must not be decorative. With a small
    // `max_upload_bytes` the body-limit layer cuts the multipart stream short,
    // so the upload is refused as an unreadable form...
    let tight = app_from(
        r#"
[document]
max_upload_bytes = 1024
"#,
    );
    assert_eq!(caps(&tight).await["limits"]["maxUploadBytes"], json!(1024));

    let tight_err = upload_error(&tight, 4096).await;
    assert!(
        tight_err.contains("invalid multipart form"),
        "an upload over the advertised cap must be cut off by the body limit, got: {tight_err}"
    );

    // ...while under the default cap the SAME bytes are read in full and fail
    // later on content. That contrast proves the rejection above was the
    // advertised size cap doing its job, not a malformed request.
    let roomy_err = upload_error(&app_from(""), 4096).await;
    assert!(
        !roomy_err.contains("invalid multipart form"),
        "under the default cap the same upload must be readable, got: {roomy_err}"
    );
}

// ── LLM: providers come from the dispatcher; formats declare their gates ─────

#[tokio::test]
async fn llm_providers_match_the_dispatcher() {
    let body = caps(&app_from("")).await;
    let providers: Vec<String> = serde_json::from_value(body["llm"]["providers"].clone()).unwrap();

    assert_eq!(
        providers,
        crw_extract::llm::SUPPORTED_PROVIDERS
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>(),
        "the advertised providers must be exactly the ones dispatch accepts"
    );
    for p in &providers {
        assert!(
            crw_extract::llm::is_supported_provider(p),
            "advertised provider `{p}` is rejected by the dispatcher"
        );
    }
}

#[tokio::test]
async fn llm_required_formats_are_declared() {
    let body = caps(&app_from("")).await;

    let supported: Vec<String> =
        serde_json::from_value(body["formats"]["supported"].clone()).unwrap();
    let llm_required: Vec<String> =
        serde_json::from_value(body["formats"]["llmRequired"].clone()).unwrap();

    // Anything credential-gated must be declared, and must be a real format.
    assert!(llm_required.iter().any(|f| f == "json"));
    assert!(llm_required.iter().any(|f| f == "summary"));
    for f in &llm_required {
        assert!(
            supported.contains(f),
            "llmRequired lists `{f}`, which is not in formats.supported"
        );
    }

    // changeTracking's json mode is LLM-backed; gitDiff is deterministic.
    let modes: Vec<String> =
        serde_json::from_value(body["formats"]["changeTrackingModes"].clone()).unwrap();
    let modes_llm: Vec<String> =
        serde_json::from_value(body["formats"]["changeTrackingModesLlmRequired"].clone()).unwrap();
    assert_eq!(modes_llm, vec!["json".to_string()]);
    for m in &modes_llm {
        assert!(modes.contains(m));
    }
    assert!(
        !modes_llm.iter().any(|m| m == "gitDiff"),
        "gitDiff is deterministic and needs no LLM"
    );
}

// ── The v2 alias serves the identical document ──────────────────────────────

#[tokio::test]
async fn v2_alias_matches_v1() {
    let server = app_from("");
    let v1: Value = server.get("/v1/capabilities").await.json();
    let v2: Value = server.get("/v2/capabilities").await.json();
    assert_eq!(v1, v2);
}
