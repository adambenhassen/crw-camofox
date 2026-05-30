//! Optional self-host **monitor mode** for CRW (Cargo feature `monitor` on
//! `crw-server`, default OFF).
//!
//! This crate gives self-hosters reduced-parity scheduled monitoring without
//! forcing a database dependency on the default stateless engine. It is the
//! self-host analogue of the SaaS control plane (§2 of the monitor plan):
//!
//! - **Store** ([`store`]) — a WAL SQLite store of monitors, targets,
//!   snapshots, checks and per-page results.
//! - **Schedule** ([`schedule`]) — a small UTC cron / fixed-interval parser.
//! - **Runner** ([`runner`]) — runs one check: scrapes/crawls, diffs each page
//!   against the stored snapshot via the pure [`crw_diff`] engine, computes
//!   **set-level** `new`/`removed` across the discovered URL set (the key
//!   self-host capability, possible because `CrawlState.data` carries the full
//!   page set), applies a site-down gate, and optionally runs the LLM judge.
//! - **Scheduler** ([`scheduler`]) — a tokio tick loop that finds due monitors
//!   and runs their checks.
//! - **Webhook** ([`webhook`]) — HMAC-SHA256 signed local webhook delivery.
//!
//! Everything here is local to this crate. The SQLite/HMAC stack is behind the
//! crate's own optional features and `crw-server` only links this crate behind
//! its `monitor` feature, so the open-core boundary (`cargo tree -p crw-server`
//! shows no `rusqlite`/`hmac`) holds.
//!
//! ## Deferred (documented TODO)
//! - SMTP email delivery is a stub ([`webhook::EmailStub`]); only HMAC webhooks
//!   are wired. SMTP balloons scope (TLS, auth, MIME, bounce handling) and is
//!   deferred to a follow-up per the M6 scope-discipline note.
//! - The `crw monitor ...` CLI surface and the MCP `monitor` tool are deferred
//!   (§9 of the plan). The library API ([`Store`], [`Scheduler`],
//!   [`run_check`]) is the integration point a CLI/endpoint would call.

pub mod config;
pub mod runner;
pub mod schedule;
pub mod scheduler;
pub mod types;
pub mod webhook;

#[cfg(feature = "store")]
pub mod store;

pub use config::MonitorConfig;
pub use runner::run_check;
pub use scheduler::Scheduler;
pub use types::{
    CheckResult, CheckStatus, Monitor, MonitorStatus, MonitorTarget, PageResult, PageStatus,
    TargetKind, WebhookConfig,
};

#[cfg(feature = "store")]
pub use store::Store;

/// Result type for monitor operations.
pub type MonitorResult<T> = Result<T, MonitorError>;

/// Errors surfaced by the monitor crate.
#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("store error: {0}")]
    Store(String),
    #[error("schedule error: {0}")]
    Schedule(String),
    #[error("scrape/crawl error: {0}")]
    Engine(String),
    #[error("webhook error: {0}")]
    Webhook(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

impl From<crw_core::error::CrwError> for MonitorError {
    fn from(e: crw_core::error::CrwError) -> Self {
        MonitorError::Engine(e.to_string())
    }
}
