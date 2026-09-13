//! What the HTTP tier does with a body that is not HTML.
//!
//! The tier used to route on the declared `Content-Type` alone: `application/pdf`
//! went to the PDF parser and EVERYTHING else was UTF-8-lossy'd into the HTML
//! extractor. So a .docx came back as `success: true` with markdown beginning
//! `PK\x03\x04...[Content_Types].xml`, and a real PDF served as
//! `application/octet-stream` (what S3 and `Content-Disposition: attachment`
//! endpoints send) came back as megabytes of raw `%PDF-1.5 ... /FlateDecode`
//! source text.
//!
//! These drive `FallbackRenderer::fetch` against a real origin, because the unit
//! tests next to `looks_binary` cover the helper only: deleting its call site,
//! or the `%PDF-` relabel, leaves every one of them green.

use std::collections::HashMap;

use crw_core::Deadline;
use crw_core::config::{RendererConfig, RendererMode, StealthConfig};
use crw_core::error::CrwError;
use crw_core::types::FetchResult;
use crw_renderer::FallbackRenderer;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn renderer() -> FallbackRenderer {
    let cfg = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    FallbackRenderer::new(&cfg, "crw-test", None, &StealthConfig::default())
        .expect("renderer builds in http-only mode")
}

/// Serve `body` under `content_type` from a throwaway origin and fetch it.
async fn fetch_body(body: Vec<u8>, content_type: &str) -> Result<FetchResult, CrwError> {
    // SAFETY: one process per tests/*.rs file, so this binary owns its env.
    unsafe {
        std::env::set_var("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1");
    }
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", content_type),
        )
        .mount(&origin)
        .await;
    renderer()
        .fetch(
            &origin.uri(),
            &HashMap::new(),
            Some(false),
            None,
            Some("auto"),
            Deadline::from_request_ms(15_000),
        )
        .await
}

/// The opening bytes of any .docx/.xlsx/.pptx, plus enough of a member name to
/// look like the real thing in a failure message.
fn zip_container() -> Vec<u8> {
    let mut b = b"PK\x03\x04\x14\x00\x08\x00\x00\x00".to_vec();
    b.extend_from_slice(b"[Content_Types].xml");
    b.extend_from_slice(&[0u8; 64]);
    b
}

#[tokio::test]
async fn a_zip_container_is_refused_instead_of_extracted_as_html() {
    let err = fetch_body(
        zip_container(),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    )
    .await
    .expect_err("a ZIP container has nothing to extract");

    match err {
        CrwError::UnsupportedContentType(msg) => {
            assert!(
                msg.contains("wordprocessingml"),
                "the caller needs the actual content type to act on: {msg}"
            );
        }
        other => panic!("expected UnsupportedContentType, got {other:?}"),
    }
}

#[tokio::test]
async fn a_pdf_served_as_octet_stream_is_relabelled_so_the_parser_engages() {
    // `crw-crawl` gates its PDF branch on `content_type == "application/pdf"`,
    // so the relabel is the whole mechanism: without it the parser never runs
    // and the caller gets the raw source instead of the document.
    let pdf =
        b"%PDF-1.5\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n";
    let r = fetch_body(pdf.to_vec(), "application/octet-stream")
        .await
        .expect("a PDF is fetchable whatever the origin calls it");

    assert_eq!(r.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(r.rendered_with.as_deref(), Some("pdf"));
    assert!(
        r.raw_bytes.is_some_and(|b| b.starts_with(b"%PDF-")),
        "the parser needs the bytes, not a decoded string"
    );
    assert!(
        r.html.is_empty(),
        "a PDF has no DOM, so nothing must be handed to the HTML extractor"
    );
}

#[tokio::test]
async fn a_declared_html_body_survives_a_stray_nul() {
    // A NUL in HTML is not a reason to refuse the page: a real browser maps it
    // to U+FFFD and renders normally, so refusing would both cost a page we
    // scrape today and hand any origin a one-byte way to shut the ladder down.
    let mut body = b"<!doctype html><html><body><h1>Still a page</h1><p>".to_vec();
    body.push(0);
    body.extend_from_slice(b"tail</p></body></html>");

    let r = fetch_body(body, "text/html; charset=utf-8")
        .await
        .expect("a NUL does not make an HTML page unscrapable");
    assert!(r.html.contains("Still a page"));
}

#[tokio::test]
async fn a_utf16_page_declared_as_unicode_still_decodes() {
    // `unicode` and `csunicode` are UTF-16LE to `encoding_rs`, which is what
    // decodes the body, and classic IIS still emits the former. A substring
    // test on the label misses both and turns the page into a hard error.
    let body: Vec<u8> = "<!doctype html><html><body><h1>Wide load</h1></body></html>"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();

    // Served as `text/plain`, NOT `text/html`, on purpose: a declared HTML-ish
    // type skips the binary check entirely, so serving it as `text/html` would
    // pass with the UTF-16 exemption deleted and prove nothing.
    for label in ["unicode", "csunicode", "utf-16le"] {
        let r = fetch_body(body.clone(), &format!("text/plain; charset={label}"))
            .await
            .unwrap_or_else(|e| panic!("charset={label} must decode, got {e:?}"));
        assert!(
            r.html.contains("Wide load"),
            "charset={label} decoded to the wrong thing"
        );
    }
}

#[tokio::test]
async fn a_binary_body_with_no_declared_type_is_still_refused() {
    // The gate skips the byte check for a DECLARED HTML-ish type. An origin
    // that declares nothing, or sends a bare `Content-Type:`, has not declared
    // HTML, and an unlabelled body is exactly what a byte sniff is for.
    for content_type in ["", "application/octet-stream"] {
        let err = fetch_body(zip_container(), content_type)
            .await
            .expect_err("an unlabelled ZIP container still has nothing to extract");
        assert!(
            matches!(err, CrwError::UnsupportedContentType(_)),
            "content-type {content_type:?} got {err:?}"
        );
    }
}
