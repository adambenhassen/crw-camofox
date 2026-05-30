//! WAL SQLite store for self-host monitors.
//!
//! Tables (all child rows cascade-delete with their `monitors` row):
//! - `monitors` — monitor config + schedule cursors.
//! - `monitor_targets` — one row per target (scrape URL set or crawl seed).
//! - `snapshots` — the latest [`ChangeTrackingSnapshot`] per `(monitor, url)`,
//!   so each diff has a `previous` baseline. Upserted on every check.
//! - `checks` — one row per check run (status + counts).
//! - `check_pages` — per-page results within a check.
//!
//! The store is `Send + Sync` via an internal `Mutex<Connection>`; the
//! scheduler is low-QPS (one writer, periodic ticks) so a single guarded
//! connection is plenty and avoids a pool dependency.

use crate::runner::PriorState;
use crate::types::{
    CheckCounts, CheckResult, CheckStatus, Monitor, MonitorStatus, MonitorTarget, PageResult,
    PageStatus, TargetKind, WebhookConfig,
};
use crate::{MonitorError, MonitorResult};
use crw_core::types::{ChangeTrackingMode, ChangeTrackingResult, ChangeTrackingSnapshot};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashSet;
use std::sync::Mutex;

/// A SQLite-backed monitor store.
pub struct Store {
    conn: Mutex<Connection>,
}

fn map_err<E: std::fmt::Display>(e: E) -> MonitorError {
    MonitorError::Store(e.to_string())
}

impl Store {
    /// Open (or create) the store at `path`, enabling WAL + foreign keys and
    /// applying the schema.
    pub fn open(path: &str) -> MonitorResult<Self> {
        let conn = Connection::open(path).map_err(map_err)?;
        Self::init(conn)
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory() -> MonitorResult<Self> {
        let conn = Connection::open_in_memory().map_err(map_err)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> MonitorResult<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_err)?;
        conn.execute_batch(SCHEMA).map_err(map_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- Monitors ----

    /// Create (insert) a monitor and its targets.
    pub fn create_monitor(
        &self,
        monitor: &Monitor,
        targets: &[MonitorTarget],
    ) -> MonitorResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(map_err)?;
        let modes_json = serde_json::to_string(&monitor.modes).map_err(map_err)?;
        let webhook_json = match &monitor.webhook {
            Some(w) => Some(serde_json::to_string(w).map_err(map_err)?),
            None => None,
        };
        tx.execute(
            "INSERT INTO monitors (id, name, status, schedule, modes, goal, judge_enabled, \
             llm_provider, llm_api_key, llm_model, webhook, next_run_at, last_run_at, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            rusqlite::params![
                monitor.id,
                monitor.name,
                monitor.status.as_str(),
                monitor.schedule,
                modes_json,
                monitor.goal,
                monitor.judge_enabled as i64,
                monitor.llm_provider,
                monitor.llm_api_key,
                monitor.llm_model,
                webhook_json,
                monitor.next_run_at,
                monitor.last_run_at,
                monitor.created_at,
            ],
        )
        .map_err(map_err)?;

        for t in targets {
            let urls_json = serde_json::to_string(&t.urls).map_err(map_err)?;
            tx.execute(
                "INSERT INTO monitor_targets (id, monitor_id, kind, urls, crawl_url, max_pages) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![
                    t.id,
                    t.monitor_id,
                    t.kind.as_str(),
                    urls_json,
                    t.crawl_url,
                    t.max_pages,
                ],
            )
            .map_err(map_err)?;
        }
        tx.commit().map_err(map_err)?;
        Ok(())
    }

    /// Delete a monitor; cascades to targets/snapshots/checks/check_pages.
    pub fn delete_monitor(&self, id: &str) -> MonitorResult<()> {
        let conn = self.lock();
        let n = conn
            .execute("DELETE FROM monitors WHERE id = ?1", [id])
            .map_err(map_err)?;
        if n == 0 {
            return Err(MonitorError::NotFound(format!("monitor {id}")));
        }
        Ok(())
    }

    /// List all monitors.
    pub fn list_monitors(&self) -> MonitorResult<Vec<Monitor>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM monitors ORDER BY created_at")
            .map_err(map_err)?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(map_err)?
            .collect::<Result<_, _>>()
            .map_err(map_err)?;
        drop(stmt);
        drop(conn);
        ids.iter().map(|id| self.get_monitor(id)).collect()
    }

    /// Get a single monitor.
    pub fn get_monitor(&self, id: &str) -> MonitorResult<Monitor> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, name, status, schedule, modes, goal, judge_enabled, llm_provider, \
             llm_api_key, llm_model, webhook, next_run_at, last_run_at, created_at \
             FROM monitors WHERE id = ?1",
            [id],
            row_to_monitor,
        )
        .optional()
        .map_err(map_err)?
        .ok_or_else(|| MonitorError::NotFound(format!("monitor {id}")))
    }

    /// Get a monitor's targets.
    pub fn get_targets(&self, monitor_id: &str) -> MonitorResult<Vec<MonitorTarget>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, monitor_id, kind, urls, crawl_url, max_pages \
                 FROM monitor_targets WHERE monitor_id = ?1 ORDER BY id",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([monitor_id], row_to_target)
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        rows.into_iter().collect::<MonitorResult<Vec<_>>>()
    }

    /// Monitors that are `active` and due at/before `now` (or never scheduled).
    pub fn due_monitors(&self, now: i64) -> MonitorResult<Vec<Monitor>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id FROM monitors WHERE status = 'active' \
                 AND (next_run_at IS NULL OR next_run_at <= ?1) ORDER BY next_run_at",
            )
            .map_err(map_err)?;
        let ids: Vec<String> = stmt
            .query_map([now], |r| r.get::<_, String>(0))
            .map_err(map_err)?
            .collect::<Result<_, _>>()
            .map_err(map_err)?;
        drop(stmt);
        drop(conn);
        ids.iter().map(|id| self.get_monitor(id)).collect()
    }

    /// Update a monitor's schedule cursors after a run.
    pub fn update_schedule(
        &self,
        id: &str,
        last_run_at: i64,
        next_run_at: i64,
    ) -> MonitorResult<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE monitors SET last_run_at = ?2, next_run_at = ?3 WHERE id = ?1",
            rusqlite::params![id, last_run_at, next_run_at],
        )
        .map_err(map_err)?;
        Ok(())
    }

    pub fn set_status(&self, id: &str, status: MonitorStatus) -> MonitorResult<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE monitors SET status = ?2 WHERE id = ?1",
            rusqlite::params![id, status.as_str()],
        )
        .map_err(map_err)?;
        Ok(())
    }

    // ---- Snapshots (prior state for diffing) ----

    /// Load the prior state for a target: the latest snapshot per URL and the
    /// full set of URLs known to this monitor (the prior discovered set).
    pub fn load_prior(&self, monitor_id: &str) -> MonitorResult<PriorState> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT url, snapshot FROM snapshots WHERE monitor_id = ?1")
            .map_err(map_err)?;
        let mut snapshots = std::collections::HashMap::new();
        let mut known_urls = HashSet::new();
        let rows = stmt
            .query_map([monitor_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(map_err)?;
        for row in rows {
            let (url, snap_json) = row.map_err(map_err)?;
            let snap: ChangeTrackingSnapshot = serde_json::from_str(&snap_json).map_err(map_err)?;
            known_urls.insert(url.clone());
            snapshots.insert(url, snap);
        }
        Ok(PriorState {
            snapshots,
            known_urls,
        })
    }

    /// Upsert the latest snapshot for `(monitor, url)`.
    pub fn save_snapshot(
        &self,
        monitor_id: &str,
        url: &str,
        snapshot: &ChangeTrackingSnapshot,
        captured_at: i64,
    ) -> MonitorResult<()> {
        let conn = self.lock();
        let snap_json = serde_json::to_string(snapshot).map_err(map_err)?;
        conn.execute(
            "INSERT INTO snapshots (monitor_id, url, snapshot, captured_at) VALUES (?1,?2,?3,?4) \
             ON CONFLICT(monitor_id, url) DO UPDATE SET snapshot = excluded.snapshot, \
             captured_at = excluded.captured_at",
            rusqlite::params![monitor_id, url, snap_json, captured_at],
        )
        .map_err(map_err)?;
        Ok(())
    }

    /// Drop the snapshot for a URL that no longer exists (removed page).
    pub fn delete_snapshot(&self, monitor_id: &str, url: &str) -> MonitorResult<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM snapshots WHERE monitor_id = ?1 AND url = ?2",
            rusqlite::params![monitor_id, url],
        )
        .map_err(map_err)?;
        Ok(())
    }

    // ---- Checks ----

    /// Persist a completed check + its pages, and update the snapshot baselines
    /// for same/changed/new pages (and drop removed pages' snapshots).
    pub fn record_check(&self, check: &CheckResult) -> MonitorResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(map_err)?;
        tx.execute(
            "INSERT INTO checks (id, monitor_id, status, started_at, completed_at, site_down, \
             count_same, count_new, count_changed, count_removed, count_error) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                check.id,
                check.monitor_id,
                check.status.as_str(),
                check.started_at,
                check.completed_at,
                check.site_down as i64,
                check.counts.same,
                check.counts.new,
                check.counts.changed,
                check.counts.removed,
                check.counts.error,
            ],
        )
        .map_err(map_err)?;

        for p in &check.pages {
            let ct_json = match &p.change_tracking {
                Some(ct) => Some(serde_json::to_string(ct).map_err(map_err)?),
                None => None,
            };
            tx.execute(
                "INSERT INTO check_pages (check_id, monitor_id, url, status, content_hash, \
                 change_tracking, error) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    check.id,
                    check.monitor_id,
                    p.url,
                    p.status.as_str(),
                    p.content_hash,
                    ct_json,
                    p.error,
                ],
            )
            .map_err(map_err)?;

            // Maintain snapshot baselines inside the same transaction.
            match p.status {
                PageStatus::Same | PageStatus::Changed | PageStatus::New => {
                    if let Some(ct) = &p.change_tracking
                        && let Some(snap) = &ct.snapshot
                    {
                        let snap_json = serde_json::to_string(snap).map_err(map_err)?;
                        tx.execute(
                            "INSERT INTO snapshots (monitor_id, url, snapshot, captured_at) \
                             VALUES (?1,?2,?3,?4) ON CONFLICT(monitor_id, url) DO UPDATE SET \
                             snapshot = excluded.snapshot, captured_at = excluded.captured_at",
                            rusqlite::params![
                                check.monitor_id,
                                p.url,
                                snap_json,
                                check.completed_at
                            ],
                        )
                        .map_err(map_err)?;
                    }
                }
                PageStatus::Removed => {
                    tx.execute(
                        "DELETE FROM snapshots WHERE monitor_id = ?1 AND url = ?2",
                        rusqlite::params![check.monitor_id, p.url],
                    )
                    .map_err(map_err)?;
                }
                PageStatus::Error => { /* keep prior snapshot untouched */ }
            }
        }
        tx.commit().map_err(map_err)?;
        Ok(())
    }

    /// Load a check + its pages (for inspection / webhook replay).
    pub fn get_check(&self, id: &str) -> MonitorResult<CheckResult> {
        let conn = self.lock();
        let mut check = conn
            .query_row(
                "SELECT id, monitor_id, status, started_at, completed_at, site_down, \
                 count_same, count_new, count_changed, count_removed, count_error \
                 FROM checks WHERE id = ?1",
                [id],
                row_to_check,
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| MonitorError::NotFound(format!("check {id}")))?;

        let mut stmt = conn
            .prepare(
                "SELECT url, status, content_hash, change_tracking, error \
                 FROM check_pages WHERE check_id = ?1 ORDER BY rowid",
            )
            .map_err(map_err)?;
        let pages = stmt
            .query_map([id], row_to_page)
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        check.pages = pages.into_iter().collect::<MonitorResult<Vec<_>>>()?;
        Ok(check)
    }

    /// List check ids for a monitor, newest first.
    pub fn list_check_ids(&self, monitor_id: &str) -> MonitorResult<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM checks WHERE monitor_id = ?1 ORDER BY started_at DESC")
            .map_err(map_err)?;
        let ids = stmt
            .query_map([monitor_id], |r| r.get::<_, String>(0))
            .map_err(map_err)?
            .collect::<Result<_, _>>()
            .map_err(map_err)?;
        Ok(ids)
    }
}

// ---- row mappers ----

fn row_to_monitor(r: &rusqlite::Row<'_>) -> rusqlite::Result<Monitor> {
    let modes_json: String = r.get(4)?;
    let modes: Vec<ChangeTrackingMode> = serde_json::from_str(&modes_json).unwrap_or_default();
    let webhook_json: Option<String> = r.get(10)?;
    let webhook: Option<WebhookConfig> = webhook_json.and_then(|s| serde_json::from_str(&s).ok());
    let status_s: String = r.get(2)?;
    Ok(Monitor {
        id: r.get(0)?,
        name: r.get(1)?,
        status: MonitorStatus::parse_str(&status_s).unwrap_or(MonitorStatus::Disabled),
        schedule: r.get(3)?,
        modes,
        goal: r.get(5)?,
        judge_enabled: r.get::<_, i64>(6)? != 0,
        llm_provider: r.get(7)?,
        llm_api_key: r.get(8)?,
        llm_model: r.get(9)?,
        webhook,
        next_run_at: r.get(11)?,
        last_run_at: r.get(12)?,
        created_at: r.get(13)?,
    })
}

fn row_to_target(r: &rusqlite::Row<'_>) -> rusqlite::Result<MonitorResult<MonitorTarget>> {
    let kind_s: String = r.get(2)?;
    let urls_json: String = r.get(3)?;
    Ok((|| {
        let kind = TargetKind::parse_str(&kind_s)
            .ok_or_else(|| MonitorError::Store(format!("bad target kind '{kind_s}'")))?;
        let urls: Vec<String> = serde_json::from_str(&urls_json).map_err(map_err)?;
        Ok(MonitorTarget {
            id: r.get(0).map_err(map_err)?,
            monitor_id: r.get(1).map_err(map_err)?,
            kind,
            urls,
            crawl_url: r.get(4).map_err(map_err)?,
            max_pages: r.get(5).map_err(map_err)?,
        })
    })())
}

fn row_to_check(r: &rusqlite::Row<'_>) -> rusqlite::Result<CheckResult> {
    let status_s: String = r.get(2)?;
    Ok(CheckResult {
        id: r.get(0)?,
        monitor_id: r.get(1)?,
        status: CheckStatus::parse_str(&status_s).unwrap_or(CheckStatus::Failed),
        started_at: r.get(3)?,
        completed_at: r.get(4)?,
        site_down: r.get::<_, i64>(5)? != 0,
        pages: Vec::new(),
        counts: CheckCounts {
            same: r.get(6)?,
            new: r.get(7)?,
            changed: r.get(8)?,
            removed: r.get(9)?,
            error: r.get(10)?,
        },
    })
}

fn row_to_page(r: &rusqlite::Row<'_>) -> rusqlite::Result<MonitorResult<PageResult>> {
    let status_s: String = r.get(1)?;
    let ct_json: Option<String> = r.get(3)?;
    Ok((|| {
        let status = PageStatus::parse_str(&status_s)
            .ok_or_else(|| MonitorError::Store(format!("bad page status '{status_s}'")))?;
        let change_tracking: Option<ChangeTrackingResult> = match ct_json {
            Some(s) => Some(serde_json::from_str(&s).map_err(map_err)?),
            None => None,
        };
        Ok(PageResult {
            url: r.get(0).map_err(map_err)?,
            status,
            content_hash: r.get(2).map_err(map_err)?,
            change_tracking,
            error: r.get(4).map_err(map_err)?,
        })
    })())
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS monitors (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    status       TEXT NOT NULL,
    schedule     TEXT NOT NULL,
    modes        TEXT NOT NULL DEFAULT '[]',
    goal         TEXT,
    judge_enabled INTEGER NOT NULL DEFAULT 0,
    llm_provider TEXT,
    llm_api_key  TEXT,
    llm_model    TEXT,
    webhook      TEXT,
    next_run_at  INTEGER,
    last_run_at  INTEGER,
    created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS monitor_targets (
    id          TEXT PRIMARY KEY,
    monitor_id  TEXT NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,
    urls        TEXT NOT NULL DEFAULT '[]',
    crawl_url   TEXT,
    max_pages   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_targets_monitor ON monitor_targets(monitor_id);

CREATE TABLE IF NOT EXISTS snapshots (
    monitor_id  TEXT NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    url         TEXT NOT NULL,
    snapshot    TEXT NOT NULL,
    captured_at INTEGER NOT NULL,
    PRIMARY KEY (monitor_id, url)
);

CREATE TABLE IF NOT EXISTS checks (
    id            TEXT PRIMARY KEY,
    monitor_id    TEXT NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    status        TEXT NOT NULL,
    started_at    INTEGER NOT NULL,
    completed_at  INTEGER NOT NULL,
    site_down     INTEGER NOT NULL DEFAULT 0,
    count_same    INTEGER NOT NULL DEFAULT 0,
    count_new     INTEGER NOT NULL DEFAULT 0,
    count_changed INTEGER NOT NULL DEFAULT 0,
    count_removed INTEGER NOT NULL DEFAULT 0,
    count_error   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_checks_monitor ON checks(monitor_id, started_at);

CREATE TABLE IF NOT EXISTS check_pages (
    check_id        TEXT NOT NULL REFERENCES checks(id) ON DELETE CASCADE,
    monitor_id      TEXT NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
    url             TEXT NOT NULL,
    status          TEXT NOT NULL,
    content_hash    TEXT,
    change_tracking TEXT,
    error           TEXT
);
CREATE INDEX IF NOT EXISTS idx_pages_check ON check_pages(check_id);
CREATE INDEX IF NOT EXISTS idx_pages_monitor_url ON check_pages(monitor_id, url);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CheckCounts;

    fn sample_monitor() -> Monitor {
        Monitor {
            id: "m1".into(),
            name: "test".into(),
            status: MonitorStatus::Active,
            schedule: "300s".into(),
            modes: vec![ChangeTrackingMode::GitDiff],
            goal: Some("price".into()),
            judge_enabled: true,
            llm_provider: Some("anthropic".into()),
            llm_api_key: Some("k".into()),
            llm_model: None,
            webhook: Some(WebhookConfig {
                url: "https://hook.example/cb".into(),
                secret: "s3cr3t".into(),
            }),
            next_run_at: Some(1000),
            last_run_at: None,
            created_at: 500,
        }
    }

    fn sample_targets() -> Vec<MonitorTarget> {
        vec![
            MonitorTarget {
                id: "t1".into(),
                monitor_id: "m1".into(),
                kind: TargetKind::Scrape,
                urls: vec!["https://ex.com/a".into(), "https://ex.com/b".into()],
                crawl_url: None,
                max_pages: None,
            },
            MonitorTarget {
                id: "t2".into(),
                monitor_id: "m1".into(),
                kind: TargetKind::Crawl,
                urls: vec![],
                crawl_url: Some("https://ex.com".into()),
                max_pages: Some(50),
            },
        ]
    }

    #[test]
    fn monitor_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let m = sample_monitor();
        let targets = sample_targets();
        store.create_monitor(&m, &targets).unwrap();

        let got = store.get_monitor("m1").unwrap();
        assert_eq!(got.id, "m1");
        assert_eq!(got.name, "test");
        assert_eq!(got.status, MonitorStatus::Active);
        assert_eq!(got.schedule, "300s");
        assert_eq!(got.modes, vec![ChangeTrackingMode::GitDiff]);
        assert_eq!(got.goal.as_deref(), Some("price"));
        assert!(got.judge_enabled);
        assert_eq!(got.webhook.as_ref().unwrap().secret, "s3cr3t");
        assert_eq!(got.next_run_at, Some(1000));

        let got_targets = store.get_targets("m1").unwrap();
        assert_eq!(got_targets.len(), 2);
        assert_eq!(got_targets[0].kind, TargetKind::Scrape);
        assert_eq!(got_targets[0].urls.len(), 2);
        assert_eq!(got_targets[1].kind, TargetKind::Crawl);
        assert_eq!(got_targets[1].max_pages, Some(50));

        let all = store.list_monitors().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn snapshot_round_trip_and_prior() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_monitor(&sample_monitor(), &sample_targets())
            .unwrap();

        let snap = ChangeTrackingSnapshot {
            markdown: Some("hello".into()),
            json: None,
            content_hash: "abc".into(),
            captured_at: None,
        };
        store
            .save_snapshot("m1", "https://ex.com/a", &snap, 1234)
            .unwrap();

        let prior = store.load_prior("m1").unwrap();
        assert_eq!(prior.known_urls.len(), 1);
        assert!(prior.known_urls.contains("https://ex.com/a"));
        assert_eq!(
            prior
                .snapshots
                .get("https://ex.com/a")
                .unwrap()
                .markdown
                .as_deref(),
            Some("hello")
        );

        // upsert overwrites
        let snap2 = ChangeTrackingSnapshot {
            markdown: Some("world".into()),
            content_hash: "def".into(),
            ..snap.clone()
        };
        store
            .save_snapshot("m1", "https://ex.com/a", &snap2, 5678)
            .unwrap();
        let prior2 = store.load_prior("m1").unwrap();
        assert_eq!(
            prior2
                .snapshots
                .get("https://ex.com/a")
                .unwrap()
                .markdown
                .as_deref(),
            Some("world")
        );
        assert_eq!(prior2.known_urls.len(), 1);
    }

    #[test]
    fn check_round_trip_updates_baselines() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_monitor(&sample_monitor(), &sample_targets())
            .unwrap();

        let new_snap = ChangeTrackingSnapshot {
            markdown: Some("v1".into()),
            json: None,
            content_hash: "h1".into(),
            captured_at: None,
        };
        let check = CheckResult {
            id: "c1".into(),
            monitor_id: "m1".into(),
            status: CheckStatus::Completed,
            started_at: 1000,
            completed_at: 1005,
            site_down: false,
            pages: vec![PageResult {
                url: "https://ex.com/a".into(),
                status: PageStatus::New,
                content_hash: Some("h1".into()),
                change_tracking: Some(ChangeTrackingResult {
                    status: crw_core::types::ChangeStatus::Changed,
                    first_observation: true,
                    content_hash: "h1".into(),
                    snapshot: Some(new_snap),
                    diff: None,
                    judgment: None,
                    tag: None,
                    truncated: false,
                }),
                error: None,
            }],
            counts: CheckCounts {
                new: 1,
                ..Default::default()
            },
        };
        store.record_check(&check).unwrap();

        // check + page round-trip
        let got = store.get_check("c1").unwrap();
        assert_eq!(got.status, CheckStatus::Completed);
        assert_eq!(got.counts.new, 1);
        assert_eq!(got.pages.len(), 1);
        assert_eq!(got.pages[0].status, PageStatus::New);

        // baseline persisted from the page's snapshot
        let prior = store.load_prior("m1").unwrap();
        assert_eq!(
            prior
                .snapshots
                .get("https://ex.com/a")
                .unwrap()
                .markdown
                .as_deref(),
            Some("v1")
        );

        let ids = store.list_check_ids("m1").unwrap();
        assert_eq!(ids, vec!["c1".to_string()]);
    }

    #[test]
    fn cascade_delete_removes_children() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_monitor(&sample_monitor(), &sample_targets())
            .unwrap();
        let snap = ChangeTrackingSnapshot {
            markdown: Some("x".into()),
            content_hash: "h".into(),
            ..Default::default()
        };
        store
            .save_snapshot("m1", "https://ex.com/a", &snap, 1)
            .unwrap();

        store.delete_monitor("m1").unwrap();
        assert!(store.get_monitor("m1").is_err());
        // children gone via cascade
        assert!(store.get_targets("m1").unwrap().is_empty());
        let prior = store.load_prior("m1").unwrap();
        assert!(prior.known_urls.is_empty());
        assert!(store.delete_monitor("m1").is_err());
    }

    #[test]
    fn due_monitors_filters_by_status_and_time() {
        let store = Store::open_in_memory().unwrap();
        let mut m = sample_monitor();
        m.next_run_at = Some(1000);
        store.create_monitor(&m, &[]).unwrap();

        // not due yet
        assert!(store.due_monitors(999).unwrap().is_empty());
        // due now
        assert_eq!(store.due_monitors(1000).unwrap().len(), 1);
        // paused → not due
        store.set_status("m1", MonitorStatus::Paused).unwrap();
        assert!(store.due_monitors(2000).unwrap().is_empty());
    }
}
