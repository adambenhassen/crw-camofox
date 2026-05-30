//! Core monitor domain types (self-host shape; a reduced-parity mirror of the
//! SaaS Prisma models in §4.1 of the plan).

use crw_core::types::{ChangeTrackingMode, ChangeTrackingResult};
use serde::{Deserialize, Serialize};

/// Whether a monitor's target is a single set of URLs (scrape) or a crawl that
/// discovers its own URL set. Set-level `removed` applies **only** to crawl
/// targets (a fixed `urls[]` scrape entry that errors is `error`, never
/// `removed`) — matching the plan's intentional new/removed boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Scrape,
    Crawl,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Scrape => "scrape",
            TargetKind::Crawl => "crawl",
        }
    }
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "scrape" => Some(TargetKind::Scrape),
            "crawl" => Some(TargetKind::Crawl),
            _ => None,
        }
    }
}

/// Lifecycle status of a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorStatus {
    Active,
    Paused,
    Disabled,
}

impl MonitorStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MonitorStatus::Active => "active",
            MonitorStatus::Paused => "paused",
            MonitorStatus::Disabled => "disabled",
        }
    }
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(MonitorStatus::Active),
            "paused" => Some(MonitorStatus::Paused),
            "disabled" => Some(MonitorStatus::Disabled),
            _ => None,
        }
    }
}

/// Outcome of one check run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Completed,
    /// Site-down gate tripped (>80% of known URLs vanished) — mass-removed
    /// suppressed, results recorded but flagged.
    Partial,
    Failed,
}

impl CheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Completed => "completed",
            CheckStatus::Partial => "partial",
            CheckStatus::Failed => "failed",
        }
    }
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "completed" => Some(CheckStatus::Completed),
            "partial" => Some(CheckStatus::Partial),
            "failed" => Some(CheckStatus::Failed),
            _ => None,
        }
    }
}

/// Per-page classification. `Same`/`Changed` come straight from opencore's
/// [`crw_core::types::ChangeStatus`]; `New`/`Removed` are set-level states the
/// runner computes by diffing discovered URL sets; `Error` is a fetch failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageStatus {
    Same,
    New,
    Changed,
    Removed,
    Error,
}

impl PageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PageStatus::Same => "same",
            PageStatus::New => "new",
            PageStatus::Changed => "changed",
            PageStatus::Removed => "removed",
            PageStatus::Error => "error",
        }
    }
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "same" => Some(PageStatus::Same),
            "new" => Some(PageStatus::New),
            "changed" => Some(PageStatus::Changed),
            "removed" => Some(PageStatus::Removed),
            "error" => Some(PageStatus::Error),
            _ => None,
        }
    }
}

/// HMAC-signed local webhook config attached to a monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookConfig {
    pub url: String,
    /// Shared secret used to sign deliveries (`X-CRW-Signature`). Stored as-is
    /// in the self-host SQLite DB (operator-owned, single-tenant).
    pub secret: String,
}

/// A monitor: a schedule + targets + diff mode + optional judge + webhook.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: String,
    pub name: String,
    pub status: MonitorStatus,
    /// UTC schedule. Either `@every <secs>s` / a plain integer (seconds), or a
    /// 5-field cron expression. See [`crate::schedule`].
    pub schedule: String,
    /// Diff modes applied to every target page.
    #[serde(default)]
    pub modes: Vec<ChangeTrackingMode>,
    /// Optional natural-language goal for the meaningful-change judge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// Whether to run the LLM judge on changed pages (needs `goal` + an LLM key).
    #[serde(default)]
    pub judge_enabled: bool,
    /// Optional per-monitor BYOK overrides for the judge (provider/key/model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookConfig>,
    /// Next due time, unix seconds (UTC). `None` until first scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<i64>,
    /// Last run time, unix seconds (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    pub created_at: i64,
}

/// One target within a monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorTarget {
    pub id: String,
    pub monitor_id: String,
    pub kind: TargetKind,
    /// Scrape targets: the fixed URL set. Crawl targets: ignored (use `crawl_url`).
    #[serde(default)]
    pub urls: Vec<String>,
    /// Crawl targets: the seed URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crawl_url: Option<String>,
    /// Crawl targets: page cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<u32>,
}

/// Result of one page within a check (persisted to `check_pages`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageResult {
    pub url: String,
    pub status: PageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// The change-tracking result for same/changed pages (carries the diff +
    /// any judgment). `None` for `new`/`removed`/`error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_tracking: Option<ChangeTrackingResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregate result of one check run (persisted to `checks` + `check_pages`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub id: String,
    pub monitor_id: String,
    pub status: CheckStatus,
    pub started_at: i64,
    pub completed_at: i64,
    /// True when the site-down gate suppressed mass-removed pages.
    #[serde(default)]
    pub site_down: bool,
    pub pages: Vec<PageResult>,
    pub counts: CheckCounts,
}

/// Per-status counters, driven solely by the per-page `status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckCounts {
    pub same: u32,
    pub new: u32,
    pub changed: u32,
    pub removed: u32,
    pub error: u32,
}

impl CheckCounts {
    pub fn tally(pages: &[PageResult]) -> Self {
        let mut c = CheckCounts::default();
        for p in pages {
            match p.status {
                PageStatus::Same => c.same += 1,
                PageStatus::New => c.new += 1,
                PageStatus::Changed => c.changed += 1,
                PageStatus::Removed => c.removed += 1,
                PageStatus::Error => c.error += 1,
            }
        }
        c
    }
}
