//! A latched host whose proxy fails, and whose origin then refuses the direct
//! rescue, must not be sent back to that same proxy.
//!
//! The connection-failure arm re-armed the proxy without checking
//! `direct_rescue_used`, so the request went direct → refused → proxy again,
//! spent the remaining budget on an egress already known to be broken, and was
//! reported as the proxy's failure (502/504) instead of the origin refusing us.
//!
//! Own test binary: it sets process-wide proxy env.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crw_core::Deadline;
use crw_core::config::{RendererConfig, RendererMode, StealthConfig};
use crw_core::error::CrwError;
use crw_renderer::FallbackRenderer;

fn set_env(k: &str, v: &str) {
    // SAFETY: one binary per tests/*.rs, so this process owns its env.
    unsafe { std::env::set_var(k, v) }
}

#[tokio::test]
async fn refused_direct_rescue_does_not_go_back_to_the_failed_proxy() {
    set_env("CRW_ALLOW_LOOPBACK_FOR_TESTS", "1");

    // Proxy: accepts and drops every connection, counting how often it is used.
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let proxy_hits = Arc::new(AtomicUsize::new(0));
    let hits = proxy_hits.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = proxy.accept().await {
            hits.fetch_add(1, Ordering::SeqCst);
            drop(sock);
        }
    });
    set_env(
        "CRW_HTTP_RATELIMIT_PROXY_URL",
        &format!("http://{proxy_addr}"),
    );

    // Origin: a closed port, so the direct rescue is refused.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_addr = closed.local_addr().unwrap();
    drop(closed);
    let url = format!("http://{origin_addr}/");

    crw_renderer::egress::global().note_block("127.0.0.1").await;
    assert!(
        crw_renderer::egress::global()
            .should_proxy("127.0.0.1")
            .await,
        "test precondition: host must be latched"
    );

    let renderer = FallbackRenderer::new(
        &RendererConfig {
            mode: RendererMode::None,
            ..Default::default()
        },
        "crw-test",
        None,
        &StealthConfig::default(),
    )
    .expect("renderer builds in http-only mode");

    let result = renderer
        .fetch(
            &url,
            &HashMap::new(),
            Some(false),
            None,
            None,
            Deadline::from_request_ms(8_000),
        )
        .await;

    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        1,
        "the failed proxy must be tried once, not again after direct is refused"
    );
    assert!(
        matches!(result, Err(CrwError::TargetUnreachable(_))),
        "a refused direct rescue must report the origin as unreachable, got {result:?}"
    );
}
