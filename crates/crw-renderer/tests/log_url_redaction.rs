//! A target URL never reaches a log line. Engine logs are shipped to an
//! external store and a URL's path, query and fragment can carry tokens,
//! signed links or emails, so every line prints the origin plus a keyed tag
//! (`crw_core::log_url`) instead.
//!
//! These drive `FallbackRenderer::fetch` against a real origin and read back
//! what `tracing` emitted. Needles are named by position only: printing one
//! would put the value under test into the test output.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crw_core::Deadline;
use crw_core::config::{RendererConfig, RendererMode, StealthConfig};
use crw_core::log_url::{LogErr, LogUrl};
use crw_renderer::FallbackRenderer;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLog {
    /// Thread-local capture: `#[tokio::test]` runs on the test thread.
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(move || writer.clone())
                .finish(),
        )
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn renderer() -> FallbackRenderer {
    let cfg = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default())
        .expect("renderer builds in http-only mode")
}

fn allow_loopback() {
    // SAFETY: one process per tests/*.rs file, so this binary owns its env.
    unsafe {
        std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1");
    }
}

fn assert_none_of(log: &str, needles: &[&str]) {
    for (i, needle) in needles.iter().enumerate() {
        assert!(!log.contains(needle), "needle #{i} leaked into the log");
    }
}

#[tokio::test]
async fn a_scrape_logs_the_origin_and_tag_never_the_path_query_or_fragment() {
    allow_loopback();
    let captured = CapturedLog::default();
    let _guard = captured.install();
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<html><body><p>hello</p></body></html>")
                .insert_header("content-type", "text/html"),
        )
        .mount(&origin)
        .await;
    let target = format!(
        "{}/reset/s3cret-seg?token=t0k3n-482913&e=a@b.example#frag-9",
        origin.uri()
    );

    renderer()
        .fetch(
            &target,
            &HashMap::new(),
            Some(false),
            None,
            Some("auto"),
            Deadline::from_request_ms(15_000),
        )
        .await
        .expect("the origin serves a page");

    let log = captured.text();
    let line = log
        .lines()
        .find(|l| l.contains("FallbackRenderer::fetch dispatching"))
        .unwrap_or_else(|| panic!("no dispatch line was logged"));
    let expected = LogUrl(&target).to_string();
    assert!(
        expected.starts_with("http://127.0.0.1:"),
        "the origin must stay visible: {expected}"
    );
    assert!(
        line.contains(&format!("url={expected}")),
        "line does not carry origin and tag: {line}"
    );
    assert_none_of(
        &log,
        &[
            "s3cret-seg",
            "t0k3n-482913",
            "a@b.example",
            "frag-9",
            "/reset",
        ],
    );
}

/// The INFO line for a binary body is written by the response tail both
/// fetchers share.
#[tokio::test]
async fn the_binary_body_line_carries_no_query() {
    allow_loopback();
    let captured = CapturedLog::default();
    let _guard = captured.install();
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"PK\x03\x04\x00\x00\x00\x00".to_vec())
                .insert_header("content-type", "application/octet-stream"),
        )
        .mount(&origin)
        .await;
    let target = format!("{}/dl/doc.bin?sig=t0k3n-77", origin.uri());

    renderer()
        .fetch(
            &target,
            &HashMap::new(),
            Some(false),
            None,
            Some("auto"),
            Deadline::from_request_ms(15_000),
        )
        .await
        .expect_err("a binary body is refused");

    let log = captured.text();
    let line = log
        .lines()
        .find(|l| l.contains("binary body, returning unsupported content type"))
        .unwrap_or_else(|| panic!("no binary body line was logged"));
    assert!(line.contains(&format!("url={}", LogUrl(&target))), "{line}");
    assert_none_of(&log, &["/dl/", "doc.bin", "t0k3n-77"]);
}

/// `Url::parse` accepts a literal space, and the scrub ends a URL at
/// whitespace. The error text must be built from a whitespace-free form of the
/// URL, or the tail (`&token=...`) would survive the scrub.
#[tokio::test]
async fn an_error_text_for_a_url_with_a_space_leaves_no_tail_after_the_scrub() {
    allow_loopback();
    // Port 1 is privileged: the connect is refused at once.
    let err = renderer()
        .fetch(
            "http://127.0.0.1:1/foo bar&token=t0k3n-31",
            &HashMap::new(),
            Some(false),
            None,
            Some("auto"),
            Deadline::from_request_ms(5_000),
        )
        .await
        .expect_err("nothing listens on port 1");

    let raw = err.to_string();
    assert!(
        raw.contains("foo%20bar"),
        "the error text must hold the encoded URL: {raw}"
    );
    let scrubbed = LogErr(&err).to_string();
    assert_none_of(&scrubbed, &["foo", "bar", "t0k3n-31"]);
}
