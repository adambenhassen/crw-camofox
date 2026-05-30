//! Self-host monitor configuration. Feature-gated `[monitor]` config section.

use serde::Deserialize;

/// Default judge page cap per check (mirrors hosted `MONITOR_JUDGE_MAX`).
pub const DEFAULT_JUDGE_MAX_PAGES: usize = 200;

/// Fraction of previously-known URLs that must vanish for the site-down gate to
/// trip (>80% → suppress mass-removed, mark the check `partial`).
pub const SITE_DOWN_VANISH_FRACTION: f64 = 0.80;

/// `[monitor]` config (only meaningful when the `monitor` feature is enabled).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MonitorConfig {
    /// Path to the SQLite DB file. Default `crw-monitor.db`.
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// How often the scheduler tick loop wakes to find due monitors (seconds).
    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,
    /// Max pages judged per check; pages beyond the cap are stored unjudged.
    #[serde(default = "default_judge_max_pages")]
    pub judge_max_pages_per_check: usize,
    /// Optional hard cap on total judge input tokens per check. `None` = no cap
    /// beyond the per-page byte truncation. Once exceeded, remaining changed
    /// pages are stored unjudged.
    #[serde(default)]
    pub judge_max_tokens_per_check: Option<u32>,
    /// Per-unit wall-clock cap (seconds) for a single scrape/crawl page so one
    /// in-process unit cannot stall the scheduler loop.
    #[serde(default = "default_unit_deadline_ms")]
    pub unit_deadline_ms: u64,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            tick_secs: default_tick_secs(),
            judge_max_pages_per_check: default_judge_max_pages(),
            judge_max_tokens_per_check: None,
            unit_deadline_ms: default_unit_deadline_ms(),
        }
    }
}

fn default_db_path() -> String {
    "crw-monitor.db".to_string()
}
fn default_tick_secs() -> u64 {
    30
}
fn default_judge_max_pages() -> usize {
    DEFAULT_JUDGE_MAX_PAGES
}
fn default_unit_deadline_ms() -> u64 {
    30_000
}
