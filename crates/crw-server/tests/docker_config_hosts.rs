//! Regression guard for issue #90: every host referenced in `config.docker.toml`
//! must resolve to a service that the reference `docker-compose.yml` actually
//! defines. Issue #90 shipped a SaaS-only hostname (`searxng-internal`) in the
//! opencore default; that name has no service/alias on the single-bridge compose
//! network, so search was permanently broken out of the box.
//!
//! This test parses both the TOML (`toml` dev-dep) and `docker-compose.yml`
//! (`yaml-rust2` dev-dep) and checks each renderer/search host against the
//! service names the compose file actually defines. Deriving the allowed set
//! from the real YAML — rather than a hardcoded list — means this guard also
//! bites if a compose service is renamed without updating the config host, not
//! just the config-side regression that caused #90.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The compose service that is the app itself, not a host a config URL targets.
const APP_SERVICE: &str = "crw";

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/crw-server; go up two levels.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("crw-server should live at <repo>/crates/crw-server")
        .to_path_buf()
}

/// Extract the host from a `scheme://host:port/...` URL string.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .unwrap_or_else(|e| panic!("config.docker.toml URL `{url}` did not parse: {e}"))
        .host_str()
        .unwrap_or_else(|| panic!("config.docker.toml URL `{url}` has no host"))
        .to_string()
}

/// Parse `docker-compose.yml` and return the set of top-level `services:` keys,
/// excluding the app service itself (`crw`) — config hosts target sidecars, not
/// the app.
fn compose_service_names(root: &std::path::Path) -> BTreeSet<String> {
    let compose_path = root.join("docker-compose.yml");
    assert!(
        compose_path.exists(),
        "expected docker-compose.yml at {} — did the repo layout change?",
        compose_path.display()
    );
    let raw = std::fs::read_to_string(&compose_path).expect("read docker-compose.yml");
    let docs = yaml_rust2::YamlLoader::load_from_str(&raw).expect("parse docker-compose.yml");
    let doc = docs.first().expect("docker-compose.yml is empty");
    let services = doc["services"]
        .as_hash()
        .expect("docker-compose.yml has no `services:` mapping");

    let names: BTreeSet<String> = services
        .keys()
        .filter_map(|k| k.as_str())
        .filter(|name| *name != APP_SERVICE)
        .map(str::to_string)
        .collect();

    assert!(
        !names.is_empty(),
        "docker-compose.yml defines no sidecar services — YAML structure changed?"
    );
    names
}

#[test]
fn docker_config_hosts_match_compose_services() {
    let config_path = repo_root().join("config.docker.toml");
    assert!(
        config_path.exists(),
        "expected config.docker.toml at {} — did the crate move relative to the repo root?",
        config_path.display()
    );

    let raw = std::fs::read_to_string(&config_path).expect("read config.docker.toml");
    let doc: toml::Value = toml::from_str(&raw).expect("parse config.docker.toml");

    // (config key path, the URL string) for every host-bearing field we ship.
    let mut hosts: Vec<(&str, String)> = Vec::new();

    let get = |table: &str, sub: &str, key: &str| -> Option<String> {
        doc.get(table)?
            .get(sub)?
            .get(key)?
            .as_str()
            .map(str::to_string)
    };

    if let Some(u) = get("renderer", "lightpanda", "ws_url") {
        hosts.push(("renderer.lightpanda.ws_url", u));
    }
    if let Some(u) = get("renderer", "chrome", "ws_url") {
        hosts.push(("renderer.chrome.ws_url", u));
    }
    // Camofox is the default heavy/search tier — its REST host must resolve to a
    // real compose service too (same #90 failure mode as the others).
    if let Some(u) = get("renderer", "camofox", "base_url") {
        hosts.push(("renderer.camofox.base_url", u));
    }
    if let Some(u) = doc
        .get("search")
        .and_then(|s| s.get("searxng_url"))
        .and_then(|v| v.as_str())
    {
        hosts.push(("search.searxng_url", u.to_string()));
    }

    assert!(
        !hosts.is_empty(),
        "no renderer/search host URLs found in config.docker.toml — did the schema change?"
    );

    let services = compose_service_names(&repo_root());

    for (field, url) in &hosts {
        let host = host_of(url);
        assert!(
            services.contains(&host),
            "config.docker.toml `{field}` host '{host}' is not a service defined in \
             docker-compose.yml (defined: {services:?}). A SaaS-only or typo'd host leaked into \
             the opencore default, or a compose service was renamed without updating the config — \
             see issue #90. Fix the host so it matches a real compose service."
        );
    }
}
