use crw_core::config::*;

#[test]
fn server_config_default_values() {
    let config = ServerConfig::default();
    assert_eq!(config.host, "0.0.0.0");
    assert_eq!(config.port, 3000);
    assert_eq!(config.request_timeout_secs, 60);
}

#[test]
fn renderer_config_default_values() {
    let config = RendererConfig::default();
    assert_eq!(config.mode, RendererMode::Auto);
    assert_eq!(config.page_timeout_ms, 30000);
    assert_eq!(config.pool_size, 4);
    assert!(config.lightpanda.is_none());
    assert!(config.playwright.is_none());
    assert!(config.chrome.is_none());
}

#[test]
fn crawler_config_default_values() {
    let config = CrawlerConfig::default();
    assert_eq!(config.max_concurrency, 10);
    assert!((config.requests_per_second - 10.0).abs() < f64::EPSILON);
    assert!(config.respect_robots_txt);
    assert!(config.user_agent.contains("Chrome/"));
    assert_eq!(config.default_max_depth, 2);
    assert_eq!(config.default_max_pages, 100);
    assert!(config.proxy.is_none());
    assert_eq!(config.job_ttl_secs, 3600);
}

#[test]
fn extraction_config_default_values() {
    let config = ExtractionConfig::default();
    assert_eq!(config.default_format, "markdown");
    assert!(config.only_main_content);
    assert!(config.llm.is_none());
}

#[test]
fn auth_config_default_empty() {
    let config = AuthConfig::default();
    assert!(config.api_keys.is_empty());
}

#[test]
fn server_config_deserialize_partial() {
    let toml_str = r#"
        port = 8080
    "#;
    let config: ServerConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.port, 8080);
    // host should fallback to default
    assert_eq!(config.host, "0.0.0.0");
    assert_eq!(config.request_timeout_secs, 60);
}

#[test]
fn crawler_config_deserialize_partial() {
    let toml_str = r#"
        max_concurrency = 20
        requests_per_second = 5.0
    "#;
    let config: CrawlerConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.max_concurrency, 20);
    assert!((config.requests_per_second - 5.0).abs() < f64::EPSILON);
    // defaults for the rest
    assert!(config.respect_robots_txt);
    assert!(config.user_agent.contains("Chrome/"));
}

#[test]
fn app_config_deserialize_empty() {
    let toml_str = "";
    let config: AppConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.port, 3000);
    assert_eq!(config.renderer.mode, RendererMode::Auto);
    assert_eq!(config.crawler.max_concurrency, 10);
}

#[test]
fn auth_config_api_keys_toml_array() {
    let toml_str = r#"
        api_keys = ["key1", "key2"]
    "#;
    let config: AuthConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.api_keys, vec!["key1", "key2"]);
}

#[test]
fn auth_config_api_keys_empty() {
    let toml_str = "";
    let config: AuthConfig = toml::from_str(toml_str).unwrap();
    assert!(config.api_keys.is_empty());
}

#[test]
fn auth_config_api_keys_json_string() {
    // JSON array as string (what env vars pass)
    let toml_str = r#"api_keys = "[\"key1\", \"key2\"]""#;
    let config: AuthConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.api_keys, vec!["key1", "key2"]);
}

#[test]
fn auth_config_api_keys_comma_separated() {
    // Comma-separated string
    let toml_str = r#"api_keys = "key1,key2""#;
    let config: AuthConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.api_keys, vec!["key1", "key2"]);
}

#[test]
fn auth_config_api_keys_comma_with_spaces() {
    // Comma-separated with spaces
    let toml_str = r#"api_keys = "key1, key2 , key3""#;
    let config: AuthConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.api_keys, vec!["key1", "key2", "key3"]);
}

#[test]
fn impersonated_config_defaults_on_with_http_timeout_fallback() {
    let config = RendererConfig::default();
    assert!(config.impersonated.enabled);
    assert!(config.impersonated.timeout_ms.is_none());
    assert_eq!(config.impersonated_timeout(), config.http_timeout());
}

#[test]
fn impersonated_config_parses_from_toml() {
    let config: AppConfig = toml::from_str(
        "[renderer]\nmode = \"none\"\n[renderer.impersonated]\nenabled = false\ntimeout_ms = 1234\n",
    )
    .unwrap();
    assert!(!config.renderer.impersonated.enabled);
    assert_eq!(config.renderer.impersonated_timeout(), 1234);
    assert!(!config.renderer.impersonated_in_chain());
}

#[test]
fn impersonated_in_chain_follows_the_cargo_feature() {
    let config = RendererConfig::default();
    assert_eq!(
        config.impersonated_in_chain(),
        cfg!(feature = "impersonated"),
        "enabled-by-default config must be inert without the feature"
    );
}

#[test]
fn ladder_min_deadline_counts_the_impersonated_tier_only_when_in_chain() {
    let mut on = RendererConfig {
        mode: RendererMode::None,
        ..Default::default()
    };
    on.impersonated.timeout_ms = Some(7_000);
    let mut off = on.clone();
    off.impersonated.enabled = false;
    let delta = on
        .min_deadline_for_full_ladder_ms()
        .saturating_sub(off.min_deadline_for_full_ladder_ms());
    if cfg!(feature = "impersonated") {
        assert_eq!(delta, 7_000);
    } else {
        assert_eq!(delta, 0);
    }
}
