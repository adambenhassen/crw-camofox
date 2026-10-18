//! In-memory cache of fetched pages.
//!
//! What is stored is the FETCH (`FetchResult`), never the extraction. Two
//! requests for the same page with different schemas share one fetch and each
//! still run their own extraction, which is the whole point: a multi-pass
//! extraction pays the network once instead of once per pass.
//!
//! The contract is published at `docs/docs/scraping.md`: `maxAge` in
//! milliseconds, one hour by default (`[page_cache] default_max_age_ms`),
//! clamped to a 24 hour ceiling, `0` always fetches, and a cached response
//! bills exactly like a fresh one.
//!
//! Freshness is per reader, not per entry. An entry records when it was fetched
//! and every reader compares that age against its own `maxAge`, so a caller who
//! accepted 24 hours can never hand a stale page to a caller who asked for one
//! minute.

use crw_core::config::PageCacheConfig;
use crw_core::types::FetchResult;
use moka::future::Cache;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Published default: an identical fetch is reused for an hour unless the
/// caller or `[page_cache] default_max_age_ms` says otherwise.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Published ceiling: a page can never be pinned for longer than a day. Also
/// the entry TTL, so nothing outlives the longest age any reader may accept.
pub const MAX_AGE_CEILING: Duration = Duration::from_secs(24 * 60 * 60);

/// Anything larger is streamed past the cache rather than evicting everything
/// else to hold one page.
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;

/// Settings applied once at boot by [`configure`].
struct Settings {
    enabled: bool,
    capacity_bytes: u64,
    default_max_age: Duration,
}

impl Settings {
    fn from_config(cfg: &PageCacheConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            capacity_bytes: cfg.capacity_mb.max(1).saturating_mul(1024 * 1024),
            default_max_age: Duration::from_millis(cfg.default_max_age_ms).min(MAX_AGE_CEILING),
        }
    }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| Settings::from_config(&PageCacheConfig::default()))
}

/// Install the `[page_cache]` settings. Idempotent: the first call wins, and a
/// cache already built keeps the capacity it was built with, so call it once at
/// boot before any request can scrape. Surfaces that never call it (CLI, tests)
/// get [`PageCacheConfig::default`].
pub fn configure(cfg: &PageCacheConfig) {
    let _ = SETTINGS.set(Settings::from_config(cfg));
}

/// A stored fetch, when it was fetched, and how much budget the fetch that
/// produced it had.
#[derive(Clone)]
struct CachedPage {
    fetched_at: Instant,
    budget_bucket: u8,
    result: Arc<FetchResult>,
}

/// Coarse bucket of the time budget a fetch ran under.
///
/// A short budget does not truncate a page, it makes the renderer ladder SKIP
/// tiers it has no time to try (`crw-renderer/src/lib.rs:3220`, `single.rs:535`
/// both bail below `MIN_TIER_BUDGET`). The result is a clean 200 from a lower
/// tier with `truncated == false`, so the store gate cannot see it.
///
/// Recording the bucket lets a reader refuse an entry that a tighter budget
/// produced: a caller with 30 seconds must not be handed the shell a caller
/// with 1.2 seconds had to settle for. The reverse is fine, since a page
/// fetched with more budget is at least as good.
pub fn budget_bucket(remaining_ms: u64) -> u8 {
    match remaining_ms {
        0..=1_999 => 0,
        2_000..=4_999 => 1,
        5_000..=14_999 => 2,
        15_000..=59_999 => 3,
        _ => 4,
    }
}

/// Everything that changes what the network returns. Anything that only
/// changes what we do with the bytes afterwards (formats, schema, selectors,
/// every LLM field) is deliberately absent: that is what makes a second pass
/// with a different schema a hit.
///
/// Request headers are absent too, on purpose: a request that carries any is
/// never cached at all (`single.rs`), so no header-bearing fetch reaches a key.
/// A per-request proxy and a per-request stealth override bypass the cache the
/// same way. `country` is absent because nothing in this fork's fetch path
/// reads it (the proxy-egress tier it selects does not exist here).
pub struct KeyInputs<'a> {
    pub url: &'a str,
    pub render_js: Option<bool>,
    pub wait_for: Option<u64>,
    pub renderer_pin: Option<&'a str>,
    /// The renderer's own user agent. The cache is process-global while a
    /// `FallbackRenderer` is not, so two renderers in one process (a mobile and
    /// a desktop one, say) must not share entries.
    pub user_agent: &'a str,
    /// Whether this request could have escalated to a browser. A fetch that
    /// escalated holds a rendered DOM, which is not what an HTTP-source reader
    /// would have received, so the two must not share an entry.
    pub may_escalate: bool,
}

/// Canonical key string. Every part is length-prefixed so no combination of
/// values can be made to collide by moving a delimiter into a value.
pub fn build_key(i: &KeyInputs<'_>) -> String {
    let mut out = String::with_capacity(i.url.len() + 64);
    let mut push = |s: &str| {
        out.push_str(&s.len().to_string());
        out.push(':');
        out.push_str(s);
        out.push('|');
    };
    push(i.url);
    push(match i.render_js {
        Some(true) => "js=1",
        Some(false) => "js=0",
        None => "js=auto",
    });
    push(&i.wait_for.map(|w| w.to_string()).unwrap_or_default());
    push(i.renderer_pin.unwrap_or(""));
    push(i.user_agent);
    push(if i.may_escalate { "esc=1" } else { "esc=0" });
    out
}

/// Clamp a caller `maxAge` to the published ceiling. `None` means the caller
/// said nothing and gets the configured default; `Some(0)` disables. A
/// disabled cache (`[page_cache] enabled = false`) answers zero for everyone,
/// which every caller already treats as "fetch live, store nothing".
pub fn effective_max_age(max_age_ms: Option<u64>) -> Duration {
    let s = settings();
    if !s.enabled {
        return Duration::ZERO;
    }
    match max_age_ms {
        None => s.default_max_age,
        Some(0) => Duration::ZERO,
        Some(ms) => Duration::from_millis(ms).min(MAX_AGE_CEILING),
    }
}

/// Bytes an entry is charged for. Weighing `html` alone would charge a 40MB PDF almost nothing, because a PDF
/// leaves `html` empty and fills `raw_bytes`.
fn entry_bytes(key: &str, r: &FetchResult) -> usize {
    key.len()
        + r.html.len()
        + r.raw_bytes.as_ref().map_or(0, |b| b.len())
        + r.captured_responses
            .iter()
            .map(|c| {
                c.body.as_ref().map_or(0, |b| b.len())
                    + c.url.len()
                    + c.request_id.len()
                    + c.mime_type.as_ref().map_or(0, |m| m.len())
            })
            .sum::<usize>()
        + r.final_url.as_ref().map_or(0, |u| u.len())
        + r.warning.as_ref().map_or(0, |w| w.len())
        + r.warnings.iter().map(String::len).sum::<usize>()
}

fn cache() -> &'static Cache<String, CachedPage> {
    static CACHE: OnceLock<Cache<String, CachedPage>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Cache::builder()
            .max_capacity(settings().capacity_bytes)
            .weigher(|k: &String, v: &CachedPage| {
                entry_bytes(k, &v.result).try_into().unwrap_or(u32::MAX)
            })
            .time_to_live(MAX_AGE_CEILING)
            .build()
    })
}

/// Read a page no older than `max_age`.
///
/// Returns an owned clone: `raw_bytes` is `take()`n downstream, so handing back
/// a shared value would leave the next PDF request with no bytes at all.
/// `elapsed_ms` is replaced with the real replay time, since it surfaces to the
/// caller as `metadata.elapsedMs` and replaying the original fetch's seconds on
/// a millisecond hit would invert the whole point of the feature.
pub async fn lookup(key: &str, max_age: Duration, reader_bucket: u8) -> Option<FetchResult> {
    if max_age.is_zero() {
        return None;
    }
    let started = Instant::now();
    let entry = cache().get(key).await?;
    if entry.fetched_at.elapsed() > max_age {
        return None;
    }
    if entry.budget_bucket < reader_bucket {
        return None;
    }
    let mut result = (*entry.result).clone();
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Some(result)
}

/// Store a fetch worth replaying.
///
/// `fetched_at` is the moment the NETWORK returned, captured at the fetch site,
/// not the moment this is called. Extraction sits between the two and can take
/// seconds, and timestamping at commit would let a slow old fetch overwrite a
/// newer one while claiming to be fresher.
///
/// The caller decides what is worth replaying: this only refuses what it can
/// see by itself, an empty body and an oversized entry. Everything else (2xx,
/// not truncated, not deadline-exceeded, not blocked, markdown actually
/// produced) is checked at the call site, where those facts exist.
pub async fn store(key: String, result: FetchResult, budget_bucket: u8, fetched_at: Instant) {
    if result.html.is_empty() && result.raw_bytes.as_ref().is_none_or(|b| b.is_empty()) {
        return;
    }
    if entry_bytes(&key, &result) > MAX_ENTRY_BYTES {
        return;
    }
    cache()
        .insert(
            key,
            CachedPage {
                fetched_at,
                budget_bucket,
                result: Arc::new(result),
            },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(url: &str) -> KeyInputs<'_> {
        KeyInputs {
            url,
            render_js: None,
            wait_for: None,
            renderer_pin: None,
            user_agent: "crw-test/0.0",
            may_escalate: true,
        }
    }

    fn fetch_result(html: &str) -> FetchResult {
        FetchResult {
            url: "https://example.com".into(),
            final_url: None,
            status_code: 200,
            html: html.into(),
            content_type: Some("text/html".into()),
            raw_bytes: None,
            rendered_with: Some("http".into()),
            elapsed_ms: 2580,
            warning: None,
            render_decision: None,
            credit_cost: 1,
            warnings: vec![],
            wall: None,
            truncated: false,
            deadline_exceeded: false,
            captured_responses: vec![],
        }
    }

    // Length prefixing exists so no value can impersonate a delimiter and make
    // two different fetches share one entry.
    #[test]
    fn a_value_containing_the_delimiter_cannot_collide() {
        assert_ne!(
            build_key(&inputs("https://example.com/a|b")),
            build_key(&inputs("https://example.com/a"))
        );
    }

    #[test]
    fn every_fetch_affecting_field_changes_the_key() {
        let base = build_key(&inputs("https://example.com"));
        let variants = [
            KeyInputs {
                render_js: Some(true),
                ..inputs("https://example.com")
            },
            KeyInputs {
                render_js: Some(false),
                ..inputs("https://example.com")
            },
            KeyInputs {
                wait_for: Some(500),
                ..inputs("https://example.com")
            },
            KeyInputs {
                renderer_pin: Some("camofox"),
                ..inputs("https://example.com")
            },
            KeyInputs {
                user_agent: "crw-mobile/1.0",
                ..inputs("https://example.com")
            },
            KeyInputs {
                may_escalate: false,
                ..inputs("https://example.com")
            },
            inputs("https://example.com/other"),
        ];
        for v in &variants {
            assert_ne!(base, build_key(v), "a fetch-affecting field was ignored");
        }
    }

    #[test]
    fn max_age_follows_the_published_contract() {
        // Unconfigured in unit tests, so the built-in default applies.
        assert_eq!(effective_max_age(None), DEFAULT_MAX_AGE);
        assert_eq!(effective_max_age(Some(0)), Duration::ZERO);
        assert_eq!(effective_max_age(Some(5_000)), Duration::from_secs(5));
        // Firecrawl's own two-day default lands above our published ceiling and
        // is clamped rather than honoured.
        assert_eq!(effective_max_age(Some(172_800_000)), MAX_AGE_CEILING);
    }

    #[tokio::test]
    async fn a_stored_page_comes_back_and_reports_the_replay_time() {
        let key = "replay-time-test".to_string();
        store(
            key.clone(),
            fetch_result("<html>hi</html>"),
            4,
            Instant::now(),
        )
        .await;
        let got = lookup(&key, DEFAULT_MAX_AGE, 0).await.expect("stored");
        assert_eq!(got.html, "<html>hi</html>");
        assert!(
            got.elapsed_ms < 2580,
            "replayed the original fetch duration ({}ms) instead of the hit",
            got.elapsed_ms
        );
    }

    // maxAge:0 is the documented "always fetch" escape hatch, so it must not
    // even look at a warm entry.
    #[tokio::test]
    async fn zero_max_age_never_reads() {
        let key = "zero-max-age-test".to_string();
        store(
            key.clone(),
            fetch_result("<html>hi</html>"),
            4,
            Instant::now(),
        )
        .await;
        assert!(lookup(&key, Duration::ZERO, 0).await.is_none());
    }

    // The reader's own tolerance decides, not the writer's. Without this a
    // caller who accepted a day could hand a stale page to one asking for a
    // minute.
    #[tokio::test]
    async fn an_entry_older_than_the_readers_max_age_is_a_miss() {
        let key = "reader-age-test".to_string();
        store(
            key.clone(),
            fetch_result("<html>hi</html>"),
            4,
            Instant::now(),
        )
        .await;
        assert!(lookup(&key, Duration::from_nanos(1), 0).await.is_none());
        assert!(lookup(&key, DEFAULT_MAX_AGE, 0).await.is_some());
    }

    #[tokio::test]
    async fn an_empty_body_is_not_worth_storing() {
        let key = "empty-body-test".to_string();
        store(key.clone(), fetch_result(""), 4, Instant::now()).await;
        assert!(lookup(&key, DEFAULT_MAX_AGE, 0).await.is_none());
    }

    #[tokio::test]
    async fn an_oversized_entry_is_not_stored() {
        let key = "oversized-test".to_string();
        store(
            key.clone(),
            fetch_result(&"x".repeat(MAX_ENTRY_BYTES + 1)),
            4,
            Instant::now(),
        )
        .await;
        assert!(lookup(&key, DEFAULT_MAX_AGE, 0).await.is_none());
    }

    // A tier skipped for lack of budget leaves no flag behind, so the only
    // defence is refusing to serve a tight-budget page to a generous caller.
    #[tokio::test]
    async fn a_tight_budget_page_is_not_served_to_a_generous_caller() {
        let key = "budget-bucket-test".to_string();
        store(
            key.clone(),
            fetch_result("<html>shell</html>"),
            0,
            Instant::now(),
        )
        .await;
        assert!(lookup(&key, DEFAULT_MAX_AGE, 4).await.is_none());
        assert!(lookup(&key, DEFAULT_MAX_AGE, 0).await.is_some());
    }

    #[test]
    fn config_knobs_map_to_settings_and_are_bounded() {
        let s = Settings::from_config(&PageCacheConfig {
            enabled: true,
            capacity_mb: 64,
            default_max_age_ms: 5_000,
        });
        assert!(s.enabled);
        assert_eq!(s.capacity_bytes, 64 * 1024 * 1024);
        assert_eq!(s.default_max_age, Duration::from_secs(5));

        // A default above the ceiling is clamped, and a zero capacity cannot
        // build a cache that evicts everything on insert.
        let s = Settings::from_config(&PageCacheConfig {
            enabled: false,
            capacity_mb: 0,
            default_max_age_ms: u64::MAX,
        });
        assert!(!s.enabled);
        assert_eq!(s.capacity_bytes, 1024 * 1024);
        assert_eq!(s.default_max_age, MAX_AGE_CEILING);
    }

    #[test]
    fn budget_buckets_are_ordered() {
        assert_eq!(budget_bucket(1_200), 0);
        assert_eq!(budget_bucket(30_000), 3);
        assert!(budget_bucket(120_000) > budget_bucket(1_200));
    }

    // A PDF leaves `html` empty and fills `raw_bytes`, so weighing html alone
    // would charge a 40MB document nothing at all.
    #[test]
    fn a_pdf_body_counts_towards_the_weight() {
        let mut r = fetch_result("");
        r.raw_bytes = Some(vec![0u8; 1024]);
        assert!(entry_bytes("k", &r) >= 1024);
    }

    // `raw_bytes` is take()n downstream, so a second reader must get its own
    // copy or the second request for a PDF returns empty markdown.
    #[tokio::test]
    async fn two_readers_each_get_their_own_pdf_bytes() {
        let key = "pdf-bytes-test".to_string();
        let mut r = fetch_result("");
        r.content_type = Some("application/pdf".into());
        r.raw_bytes = Some(vec![7u8; 32]);
        store(key.clone(), r, 4, Instant::now()).await;

        let mut first = lookup(&key, DEFAULT_MAX_AGE, 0).await.expect("stored");
        assert_eq!(first.raw_bytes.take().map(|b| b.len()), Some(32));
        let second = lookup(&key, DEFAULT_MAX_AGE, 0)
            .await
            .expect("still stored");
        assert_eq!(second.raw_bytes.map(|b| b.len()), Some(32));
    }
}
