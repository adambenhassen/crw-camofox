//! Per-host Cloudflare clearance cache.
//!
//! When the Camofox tier renders a page and the browser earned a
//! `cf_clearance` cookie, the tab's cookies and user agent are stored here
//! keyed by registrable host ([`crate::preference::normalize_host`]). The
//! failover ladder reads the entry before its HTTP-tier fetch and injects
//! `Cookie` + `User-Agent`, so the next scrape of that host skips Firefox.
//!
//! Only the Camofox tier writes; only the ladder reads. Entries expire with
//! the earliest cookie expiry, capped at [`MAX_TTL`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use moka::future::Cache;
use serde::Deserialize;

use crate::preference::normalize_host;

/// The cookie that proves a Cloudflare challenge was passed.
pub const CLEARANCE_COOKIE: &str = "cf_clearance";

/// Longest an entry lives regardless of cookie expiry.
pub const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Distinct hosts tracked.
pub const DEFAULT_CAPACITY: u64 = 10_000;

fn root_path() -> String {
    "/".to_string()
}

fn no_expiry() -> f64 {
    -1.0
}

/// One browser cookie as camofox-browser (Playwright) reports it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default = "root_path")]
    pub path: String,
    /// Unix seconds. `-1` (Playwright's convention) or absent = session cookie.
    #[serde(default = "no_expiry")]
    pub expires: f64,
}

/// Cookies + user agent captured from one successful Camofox render.
#[derive(Debug, Clone)]
pub struct Clearance {
    pub cookies: Vec<Cookie>,
    pub user_agent: String,
    pub expires_at: Instant,
}

impl Clearance {
    /// Build an entry from a tab's cookie jar. `None` when there is no
    /// `cf_clearance` cookie (nothing worth caching) or it already expired.
    /// TTL = earliest positive cookie expiry, capped at [`MAX_TTL`]; cookies
    /// without an expiry inherit the cap.
    pub fn from_browser(cookies: Vec<Cookie>, user_agent: String, now_unix: f64) -> Option<Self> {
        if !cookies.iter().any(|c| c.name == CLEARANCE_COOKIE) {
            return None;
        }
        let mut ttl = MAX_TTL;
        for c in &cookies {
            if c.expires > 0.0 {
                let left = c.expires - now_unix;
                if left <= 0.0 {
                    return None;
                }
                ttl = ttl.min(Duration::from_secs_f64(left));
            }
        }
        Some(Self {
            cookies,
            user_agent,
            expires_at: Instant::now() + ttl,
        })
    }

    /// `Cookie:` header value for `host`: every cached cookie whose domain
    /// covers the host, joined `name=value; `.
    pub fn cookie_header(&self, host: &str) -> String {
        self.cookies
            .iter()
            .filter(|c| cookie_matches_host(&c.domain, host))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Whether the cached `cf_clearance` applies to `host`. The cache is keyed
    /// by registrable host, so an entry captured on a sibling subdomain with a
    /// host-only cookie is found for `host` without holding a cookie for it.
    pub fn covers(&self, host: &str) -> bool {
        self.cookies
            .iter()
            .any(|c| c.name == CLEARANCE_COOKIE && cookie_matches_host(&c.domain, host))
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// RFC 6265 domain match: `.example.com` / `example.com` cover
/// `example.com` and any subdomain. An empty domain matches everything.
pub fn cookie_matches_host(domain: &str, host: &str) -> bool {
    let d = domain.trim().trim_start_matches('.').to_ascii_lowercase();
    if d.is_empty() {
        return true;
    }
    let h = host.trim().to_ascii_lowercase();
    h == d || h.ends_with(&format!(".{d}"))
}

/// Bounded async cache of [`Clearance`] per registrable host.
pub struct ClearanceCache {
    cache: Cache<String, Arc<Clearance>>,
}

impl Default for ClearanceCache {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl ClearanceCache {
    pub fn with_defaults() -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(DEFAULT_CAPACITY)
                .time_to_live(MAX_TTL)
                .build(),
        }
    }

    /// Live entry for `host`, or `None`. An entry past its own expiry is
    /// dropped on read.
    pub async fn get(&self, host: &str) -> Option<Arc<Clearance>> {
        let key = normalize_host(host);
        let entry = self.cache.get(&key).await?;
        if entry.expired() {
            self.cache.invalidate(&key).await;
            return None;
        }
        Some(entry)
    }

    pub async fn insert(&self, host: &str, clearance: Clearance) {
        self.cache
            .insert(normalize_host(host), Arc::new(clearance))
            .await;
    }

    pub async fn invalidate(&self, host: &str) {
        self.cache.invalidate(&normalize_host(host)).await;
    }

    /// Number of cached hosts (tests, admin).
    pub fn len(&self) -> u64 {
        self.cache.entry_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, domain: &str, expires: f64) -> Cookie {
        Cookie {
            name: name.into(),
            value: format!("{name}-v"),
            domain: domain.into(),
            path: "/".into(),
            expires,
        }
    }

    #[test]
    fn requires_cf_clearance() {
        assert!(
            Clearance::from_browser(vec![cookie("__cf_bm", ".a.com", -1.0)], "ua".into(), 0.0)
                .is_none()
        );
        assert!(
            Clearance::from_browser(
                vec![cookie("cf_clearance", ".a.com", -1.0)],
                "ua".into(),
                0.0
            )
            .is_some()
        );
    }

    #[test]
    fn ttl_is_min_expiry_capped() {
        let now = 1_000.0;
        let c = Clearance::from_browser(
            vec![
                cookie("cf_clearance", ".a.com", now + 600.0),
                cookie("x", ".a.com", now + 30.0),
            ],
            "ua".into(),
            now,
        )
        .unwrap();
        let ttl = c.expires_at - Instant::now();
        assert!(
            ttl <= Duration::from_secs(30) && ttl > Duration::from_secs(28),
            "ttl={ttl:?}"
        );

        let c = Clearance::from_browser(
            vec![cookie("cf_clearance", ".a.com", -1.0)],
            "ua".into(),
            now,
        )
        .unwrap();
        let ttl = c.expires_at - Instant::now();
        assert!(ttl > MAX_TTL - Duration::from_secs(2));

        let c = Clearance::from_browser(
            vec![cookie("cf_clearance", ".a.com", now + 10.0 * 24.0 * 3600.0)],
            "ua".into(),
            now,
        )
        .unwrap();
        assert!(c.expires_at - Instant::now() <= MAX_TTL);
    }

    #[test]
    fn already_expired_cookie_is_not_cached() {
        assert!(
            Clearance::from_browser(
                vec![cookie("cf_clearance", ".a.com", 5.0)],
                "ua".into(),
                10.0
            )
            .is_none()
        );
    }

    #[test]
    fn domain_matching() {
        assert!(cookie_matches_host(".example.com", "example.com"));
        assert!(cookie_matches_host(".example.com", "www.example.com"));
        assert!(cookie_matches_host("example.com", "a.b.example.com"));
        assert!(!cookie_matches_host(".example.com", "notexample.com"));
        assert!(!cookie_matches_host("www.example.com", "example.com"));
        assert!(cookie_matches_host("", "anything.test"));
    }

    #[test]
    fn cookie_header_filters_by_host() {
        let c = Clearance::from_browser(
            vec![
                cookie("cf_clearance", ".a.com", -1.0),
                cookie("other", ".b.com", -1.0),
            ],
            "ua".into(),
            0.0,
        )
        .unwrap();
        assert_eq!(c.cookie_header("www.a.com"), "cf_clearance=cf_clearance-v");
    }

    #[test]
    fn host_only_clearance_does_not_cover_a_sibling_subdomain() {
        let c = Clearance::from_browser(
            vec![cookie("cf_clearance", "www.a.com", -1.0)],
            "ua".into(),
            0.0,
        )
        .unwrap();
        assert!(c.covers("www.a.com"));
        assert!(!c.covers("a.com"));
        assert!(!c.covers("shop.a.com"));
    }

    #[tokio::test]
    async fn cache_round_trip_normalises_host() {
        let cache = ClearanceCache::with_defaults();
        let c = Clearance::from_browser(
            vec![cookie("cf_clearance", ".a.com", -1.0)],
            "ua".into(),
            0.0,
        )
        .unwrap();
        cache.insert("www.a.com", c).await;
        assert!(cache.get("a.com").await.is_some());
        assert!(cache.get("sub.a.com").await.is_some(), "eTLD+1 key");
        cache.invalidate("A.COM").await;
        assert!(cache.get("a.com").await.is_none());
    }

    #[tokio::test]
    async fn expired_entry_dropped_on_read() {
        let cache = ClearanceCache::with_defaults();
        let c = Clearance {
            cookies: vec![cookie("cf_clearance", ".a.com", -1.0)],
            user_agent: "ua".into(),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        cache.insert("a.com", c).await;
        assert!(cache.get("a.com").await.is_none());
    }
}
