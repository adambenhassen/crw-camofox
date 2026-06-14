//! Operator-facing diagnostics helpers (issue #90).
//!
//! `searxng_url` is operator-set and can carry secrets (`https://user:pass@host`,
//! a `?token=…`, or a token embedded in the path of a reverse-proxy URL). Anything
//! we log or return in an error must be sanitized to the bare origin first.

use crw_core::config::SearchConfig;
use tracing::Level;

/// Reduce a URL to its **origin** — `scheme://host[:port]` — dropping userinfo,
/// path, query, and fragment. This is the only form safe to log or echo in an
/// error, because every other component can carry a secret. Falls back to a
/// fixed redaction string if the URL doesn't parse.
pub fn sanitize_url_origin(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}", u.scheme()),
            (Some(host), None) => format!("{}://{host}", u.scheme()),
            (None, _) => "<redacted-url>".to_string(),
        },
        Err(_) => "<redacted-url>".to_string(),
    }
}

/// One-line summary of the search subsystem's configured state, for the startup
/// log. `camofox_base_url` is the configured `[renderer.camofox]` endpoint, if
/// any — it is the default backend and takes precedence over SearXNG. The states:
///   - `enabled = false`              → intentionally off
///   - camofox configured             → active via Camofox (Google)
///   - else searxng_url set           → active via SearXNG
///   - else                           → misconfigured (every call will 503)
pub fn search_startup_status(
    cfg: &SearchConfig,
    camofox_base_url: Option<&str>,
) -> (Level, String) {
    if !cfg.enabled {
        (
            Level::INFO,
            "search: disabled ([search].enabled = false)".to_string(),
        )
    } else if let Some(url) = camofox_base_url {
        (
            Level::INFO,
            format!("search: enabled (camofox={})", sanitize_url_origin(url)),
        )
    } else if let Some(url) = &cfg.searxng_url {
        (
            Level::INFO,
            format!("search: enabled (searxng={})", sanitize_url_origin(url)),
        )
    } else {
        (
            Level::WARN,
            "search: enabled but no [renderer.camofox] or [search].searxng_url — \
             /v1/search will return 503"
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_userinfo_path_query_fragment() {
        assert_eq!(
            sanitize_url_origin("https://user:pass@host:9000/searxng/tok123?q=x#frag"),
            "https://host:9000"
        );
        assert_eq!(
            sanitize_url_origin("http://searxng:8080"),
            "http://searxng:8080"
        );
        assert_eq!(
            sanitize_url_origin("http://searxng:8080/"),
            "http://searxng:8080"
        );
        // Default port is preserved as written only if explicit; no port → no port.
        assert_eq!(
            sanitize_url_origin("https://example.com"),
            "https://example.com"
        );
    }

    #[test]
    fn sanitize_redacts_unparseable() {
        assert_eq!(sanitize_url_origin("not a url"), "<redacted-url>");
    }

    fn cfg(enabled: bool, url: Option<&str>) -> SearchConfig {
        let toml = match url {
            Some(u) => format!("enabled = {enabled}\nsearxng_url = \"{u}\"\n"),
            None => format!("enabled = {enabled}\n"),
        };
        toml::from_str(&toml).expect("valid SearchConfig")
    }

    #[test]
    fn startup_status_enabled_with_url() {
        let (level, msg) = search_startup_status(&cfg(true, Some("http://searxng:8080")), None);
        assert_eq!(level, Level::INFO);
        assert!(
            msg.contains("enabled (searxng=http://searxng:8080)"),
            "{msg}"
        );
    }

    #[test]
    fn startup_status_camofox_takes_precedence() {
        // Camofox configured + searxng_url set → Camofox wins in the log.
        let (level, msg) = search_startup_status(
            &cfg(true, Some("http://searxng:8080")),
            Some("http://camofox:9377"),
        );
        assert_eq!(level, Level::INFO);
        assert!(
            msg.contains("enabled (camofox=http://camofox:9377)"),
            "{msg}"
        );
    }

    #[test]
    fn startup_status_enabled_no_backend_warns() {
        let (level, msg) = search_startup_status(&cfg(true, None), None);
        assert_eq!(level, Level::WARN);
        assert!(msg.contains("no [renderer.camofox]"), "{msg}");
    }

    #[test]
    fn startup_status_disabled() {
        let (level, msg) = search_startup_status(&cfg(false, Some("http://searxng:8080")), None);
        assert_eq!(level, Level::INFO);
        assert!(msg.contains("disabled"), "{msg}");
    }

    #[test]
    fn startup_status_never_leaks_credentials() {
        let (_, msg) =
            search_startup_status(&cfg(true, Some("https://u:secret@host:8080/tok")), None);
        assert!(!msg.contains("secret"), "{msg}");
        assert!(!msg.contains("tok"), "{msg}");
        assert!(msg.contains("https://host:8080"), "{msg}");
    }
}
