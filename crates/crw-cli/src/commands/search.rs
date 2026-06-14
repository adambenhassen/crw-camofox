//! Search subcommand — web search via SearXNG.
//!
//! This is NEW functionality that was previously only available via the REST API.

use clap::{Args, ValueEnum};
use crw_core::types::SearchEngine;
use crw_search::{CamofoxSearchClient, SearxngClient, SearxngParams, transform_flat};
use std::sync::Arc;
use std::time::Duration;

/// Max engines per search — mirrors the server-side `/v1/search` cap.
const MAX_ENGINES: usize = 4;

#[derive(Clone, Copy, ValueEnum)]
pub enum SearchFormat {
    /// JSON output with full result details
    Json,
    /// Concise text output (title + URL per line)
    Text,
    /// Markdown output with links
    Markdown,
}

#[derive(Args)]
#[command(after_help = "EXAMPLES:\n  \
    # Plain text (default)\n  \
    crw search \"rust web scraper\"\n\n  \
    # One-shot LLM-ready JSON (title + url + snippet only)\n  \
    crw search \"renewable energy 2024\" --json --fields title,url,snippet --limit 3\n\n  \
    # Save to file\n  \
    crw search \"climate news\" --json -o results.json\n\n  \
    # Multi-engine via the Camofox backend (needs camofox-browser running)\n  \
    crw search \"rust async\" --engines google,bing,reddit --camofox-url http://127.0.0.1:9377\n")]
pub struct SearchArgs {
    /// Search query
    pub query: String,

    /// Maximum number of results to return
    #[arg(short, long, default_value = "10")]
    pub limit: u32,

    /// SearXNG instance URL.
    ///
    /// Resolution order: this flag > `CRW_SEARXNG_URL` env > `search.searxng_url`
    /// in `~/.config/crw/config.toml` > `http://127.0.0.1:8080` (the default
    /// `crw setup --local` SearXNG sidecar). Public instances (searx.be, etc.)
    /// usually block JSON requests with 403/429 — prefer a local sidecar.
    #[arg(long, env = "CRW_SEARXNG_URL")]
    pub searxng_url: Option<String>,

    /// Camofox-browser REST base URL (e.g. http://127.0.0.1:9377).
    ///
    /// Selects the Camofox search backend (browser-driven Google + multi-engine).
    /// Required for `--engines`. Resolution: this flag > `CRW_CAMOFOX_URL` env >
    /// `renderer.camofox.base_url` in config. When unset, search uses SearXNG.
    #[arg(long, env = "CRW_CAMOFOX_URL")]
    pub camofox_url: Option<String>,

    /// Camofox engines to query, comma-separated (Camofox backend only).
    ///
    /// Valid: google, bing, duckduckgo, wikipedia, youtube, reddit, amazon, github.
    /// Omit for Google. Multiple are merged/deduped by URL (max 4). Requires a
    /// Camofox URL (`--camofox-url` / `CRW_CAMOFOX_URL` / config); ignored by SearXNG.
    #[arg(long, value_name = "LIST")]
    pub engines: Option<String>,

    /// Output format
    #[arg(short, long, value_enum, default_value = "text")]
    pub format: SearchFormat,

    /// Shorthand for `--format json`. Industry-standard alias (gh, kubectl,
    /// docker, jq). Wins over `--format` when both are supplied.
    #[arg(long, conflicts_with = "format")]
    pub json: bool,

    /// Write output to file instead of stdout
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<String>,

    /// Search category (general, images, news, videos, etc.)
    #[arg(long)]
    pub category: Option<String>,

    /// Language code (e.g., en, de, fr)
    #[arg(long)]
    pub language: Option<String>,

    /// Time range filter (day, week, month, year)
    #[arg(long)]
    pub time_range: Option<String>,

    /// Safe search level (0 = off, 1 = moderate, 2 = strict)
    #[arg(long)]
    pub safesearch: Option<u8>,

    /// Request timeout in seconds
    #[arg(long, default_value = "30")]
    pub timeout: u64,

    /// Project JSON output to a comma-separated subset of fields
    /// (e.g. `--fields title,url,snippet`). Only applies to `--format json`.
    /// Available: title, url, description, snippet, position, score, category.
    #[arg(long, value_name = "LIST")]
    pub fields: Option<String>,
}

pub async fn run(args: SearchArgs) {
    let http = Arc::new(
        reqwest::Client::builder()
            .redirect(crw_core::url_safety::safe_redirect_policy())
            .build()
            .expect("failed to build HTTP client"),
    );

    let timeout = Duration::from_secs(args.timeout);

    // Parse --engines (Camofox backend only); default to Google.
    let camofox_engines = match args.engines.as_deref() {
        Some(s) => match parse_engines(s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        },
        None => vec![SearchEngine::Google],
    };
    if camofox_engines.len() > MAX_ENGINES {
        eprintln!(
            "error: too many engines ({}); max {MAX_ENGINES} per search",
            camofox_engines.len()
        );
        std::process::exit(2);
    }

    let params = SearxngParams {
        q: args.query.clone(),
        categories: args.category,
        language: args.language,
        time_range: args.time_range,
        engines: None,
        pageno: None,
        safesearch: args.safesearch,
        // Read by the Camofox backend; ignored by the SearXNG client.
        camofox_engines,
    };

    // Backend selection: Camofox when a camofox URL resolves (flag/env/config);
    // otherwise SearXNG. `--engines` is Camofox-only, so it errors without a URL.
    let cfg = crw_core::config::AppConfig::load().ok();
    let response = if let Some(url) = resolve_camofox_url(args.camofox_url.as_deref(), cfg.as_ref())
    {
        let (api_key, github_token) = cfg
            .as_ref()
            .map(|c| {
                (
                    c.renderer.camofox.as_ref().and_then(|e| e.api_key.clone()),
                    c.search.github_token.clone(),
                )
            })
            .unwrap_or((None, None));
        let client = CamofoxSearchClient::new(url, api_key, github_token, timeout);
        match client.fetch(&params).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: search failed: {e}");
                eprintln!();
                eprintln!("hint: the Camofox backend (camofox-browser REST) is unreachable.");
                eprintln!("      Ensure camofox-browser is running and the URL is correct");
                eprintln!("      (--camofox-url / CRW_CAMOFOX_URL / renderer.camofox.base_url).");
                std::process::exit(1);
            }
        }
    } else if args.engines.is_some() {
        eprintln!("error: --engines needs the Camofox backend, but no camofox URL is set.");
        eprintln!("      Set --camofox-url, CRW_CAMOFOX_URL, or renderer.camofox.base_url.");
        std::process::exit(2);
    } else {
        let searxng_url = resolve_searxng_url(args.searxng_url.as_deref());
        let client = SearxngClient::new(http, &searxng_url, timeout);
        match client.fetch(&params).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: search failed: {e}");
                eprintln!();
                eprintln!(
                    "hint: SearXNG (the search backend) is unreachable at {}",
                    searxng_url
                );
                eprintln!();
                eprintln!("      Easiest fix — let `crw setup` boot a local one for you:");
                eprintln!("          crw setup --local");
                eprintln!();
                eprintln!("      Manual fix — boot SearXNG with JSON output enabled (the stock");
                eprintln!("      image ships with JSON disabled, which causes 403s):");
                eprintln!("          docker run -d --name searxng -p 8080:8080 \\");
                eprintln!(
                    "            -v ~/.config/crw/searxng-settings.yml:/etc/searxng/settings.yml \\"
                );
                eprintln!("            searxng/searxng");
                eprintln!();
                eprintln!("      Public instances (searx.be, priv.au, etc.) usually block JSON");
                eprintln!("      requests with 403/429 and are not recommended.");
                std::process::exit(1);
            }
        }
    };

    // Transform to flat result format
    let results = transform_flat(&response, args.limit);

    // `--json` shorthand wins over `--format` (clap enforces no double-set
    // via conflicts_with, but if only --json is passed we still need to
    // route to the JSON renderer).
    let format = if args.json {
        SearchFormat::Json
    } else {
        args.format
    };

    let rendered = match format {
        SearchFormat::Json => {
            // `description` is the canonical body field; `snippet` is emitted as
            // an alias so downstream LLM pipelines that ask for "snippet" don't
            // need a rename step. `--fields` projects to a user-chosen subset.
            let selected: Option<Vec<String>> = args.fields.as_ref().map(|s| {
                s.split(',')
                    .map(|f| f.trim().to_string())
                    .filter(|f| !f.is_empty())
                    .collect()
            });
            let enriched: Vec<serde_json::Value> = results
                .iter()
                .map(|r| {
                    let mut obj = serde_json::Map::new();
                    let mut insert = |k: &str, v: serde_json::Value| {
                        if let Some(ref keep) = selected {
                            if keep.iter().any(|f| f == k) {
                                obj.insert(k.to_string(), v);
                            }
                        } else {
                            obj.insert(k.to_string(), v);
                        }
                    };
                    insert("title", serde_json::json!(r.title));
                    insert("url", serde_json::json!(r.url));
                    insert("description", serde_json::json!(r.description));
                    insert("snippet", serde_json::json!(r.description));
                    insert("position", serde_json::json!(r.position));
                    insert("score", serde_json::json!(r.score));
                    insert("category", serde_json::json!(r.category));
                    serde_json::Value::Object(obj)
                })
                .collect();
            match serde_json::to_string_pretty(&enriched) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: failed to serialize JSON: {e}");
                    std::process::exit(1);
                }
            }
        }
        SearchFormat::Text => {
            if results.is_empty() {
                format!("No results found for: {}", args.query)
            } else {
                let mut out = String::new();
                for result in &results {
                    out.push_str(&result.title);
                    out.push('\n');
                    out.push_str("  ");
                    out.push_str(&result.url);
                    out.push('\n');
                    if !result.description.is_empty() {
                        let truncated: String = result.description.chars().take(200).collect();
                        out.push_str("  ");
                        if truncated.len() < result.description.len() {
                            out.push_str(&truncated);
                            out.push_str("...");
                        } else {
                            out.push_str(&result.description);
                        }
                        out.push('\n');
                    }
                    out.push('\n');
                }
                out
            }
        }
        SearchFormat::Markdown => {
            if results.is_empty() {
                format!("No results found for: {}", args.query)
            } else {
                let mut out = format!("# Search results for: {}\n\n", args.query);
                for (i, result) in results.iter().enumerate() {
                    out.push_str(&format!("{}. [{}]({})\n", i + 1, result.title, result.url));
                    if !result.description.is_empty() {
                        out.push_str(&format!("   > {}\n", result.description));
                    }
                    out.push('\n');
                }
                out
            }
        }
    };

    match args.output {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, &rendered) {
                eprintln!("error: failed to write {path}: {e}");
                std::process::exit(1);
            }
        }
        None => print!("{rendered}"),
    }
}

/// Pick the SearXNG URL from (in priority order):
///   1. CLI flag / env (already merged by clap into `cli`)
///   2. `search.searxng_url` from `~/.config/crw/config.toml`
///   3. The hardcoded `http://localhost:8080` fallback
///
/// Step 2 is what makes `crw setup` -> `crw search` work without the user
/// having to `source ~/.zshrc` first.
fn resolve_searxng_url(cli: Option<&str>) -> String {
    if let Some(url) = cli {
        return url.to_string();
    }
    if let Ok(cfg) = crw_core::config::AppConfig::load()
        && let Some(url) = cfg.search.searxng_url
    {
        return url;
    }
    // Prefer 127.0.0.1 over "localhost" — on some systems (macOS in particular)
    // "localhost" resolves to ::1 first, and a v4-only SearXNG container fails
    // with a misleading transport error before the v6→v4 fallback retries.
    "http://127.0.0.1:8080".to_string()
}

/// Resolve the Camofox-browser base URL from (in priority order):
///   1. CLI flag / `CRW_CAMOFOX_URL` env (already merged by clap into `cli`)
///   2. `renderer.camofox.base_url` from config
///
/// Returns `None` when no Camofox endpoint is configured, which routes search
/// through SearXNG instead.
fn resolve_camofox_url(
    cli: Option<&str>,
    cfg: Option<&crw_core::config::AppConfig>,
) -> Option<String> {
    if let Some(url) = cli {
        return Some(url.to_string());
    }
    cfg.and_then(|c| c.renderer.camofox.as_ref().map(|e| e.base_url.clone()))
}

/// Parse a comma-separated `--engines` list into typed engines, reusing the
/// `SearchEngine` serde names (`google`, `duckduckgo`, …). Empty tokens are
/// skipped; an unknown name is a hard error naming the offending token.
fn parse_engines(s: &str) -> Result<Vec<SearchEngine>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| {
            serde_json::from_value::<SearchEngine>(serde_json::Value::String(t.to_string()))
                .map_err(|_| {
                    format!(
                        "unknown search engine '{t}' (valid: google, bing, duckduckgo, \
                         wikipedia, youtube, reddit, amazon, github)"
                    )
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_engines_maps_known_names() {
        assert_eq!(
            parse_engines("google, bing ,duckduckgo").unwrap(),
            vec![
                SearchEngine::Google,
                SearchEngine::Bing,
                SearchEngine::DuckDuckGo
            ]
        );
    }

    #[test]
    fn parse_engines_skips_empty_tokens() {
        assert_eq!(
            parse_engines("github,,").unwrap(),
            vec![SearchEngine::Github]
        );
    }

    #[test]
    fn parse_engines_rejects_unknown() {
        let err = parse_engines("google,nope").unwrap_err();
        assert!(
            err.contains("nope"),
            "error should name the bad token: {err}"
        );
    }

    #[test]
    fn resolve_camofox_url_prefers_cli_then_none() {
        assert_eq!(
            resolve_camofox_url(Some("http://x:9377"), None),
            Some("http://x:9377".to_string())
        );
        assert_eq!(resolve_camofox_url(None, None), None);
    }
}
