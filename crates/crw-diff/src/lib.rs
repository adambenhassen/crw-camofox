//! Stateless change-tracking diff engine for CRW monitors.
//!
//! Pure, synchronous, no I/O, no LLM. Given the current scrape (markdown +
//! optionally extracted JSON) and a caller-supplied `previous` snapshot, it
//! classifies the page (`same` / `changed`), computes the requested diff
//! surfaces, and returns the current snapshot to persist as the next baseline.
//!
//! ## Caller-supplied JSON invariant
//! `current_json` is the *already-extracted* structured JSON supplied by the
//! orchestration layer. This crate NEVER extracts JSON itself and does not
//! depend on `crw-extract` — the LLM/judge live upstream.
//!
//! ## Mode-aware hashing
//! `content_hash` is the normalized-markdown hash in gitDiff/mixed mode, and
//! the canonicalized tracked-JSON hash in json-only mode. The SaaS store-skip
//! short-circuit keys off this hash.

pub mod git_diff;
pub mod json_diff;
pub mod snapshot;

use crw_core::types::{
    ChangeDiff, ChangeStatus, ChangeTrackingMode, ChangeTrackingOptions, ChangeTrackingResult,
    ChangeTrackingSnapshot,
};
use serde_json::Value;

/// Default cap on AST change-lines before the diff AST is truncated.
pub const DEFAULT_MAX_DIFF_CHANGES: usize = 5000;

/// Tunable limits for diff computation.
#[derive(Debug, Clone, Copy)]
pub struct DiffLimits {
    pub max_diff_changes: usize,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            max_diff_changes: DEFAULT_MAX_DIFF_CHANGES,
        }
    }
}

/// Compute change tracking with default limits. See module docs for the
/// caller-supplied-JSON invariant.
pub fn compute_change_tracking(
    opts: &ChangeTrackingOptions,
    current_markdown: &str,
    current_json: Option<&Value>,
    content_type: Option<&str>,
) -> ChangeTrackingResult {
    compute_change_tracking_with_limits(
        opts,
        current_markdown,
        current_json,
        content_type,
        &DiffLimits::default(),
    )
}

/// Compute change tracking with explicit limits.
pub fn compute_change_tracking_with_limits(
    opts: &ChangeTrackingOptions,
    current_markdown: &str,
    current_json: Option<&Value>,
    content_type: Option<&str>,
    limits: &DiffLimits,
) -> ChangeTrackingResult {
    let has_git = opts.modes.is_empty() || opts.modes.contains(&ChangeTrackingMode::GitDiff);
    let has_json = opts.modes.contains(&ChangeTrackingMode::Json);
    let json_only = has_json && !has_git;

    // ---- Binary / non-text content: hash only, never diff or judge ----
    if !is_text(content_type) {
        return binary_result(opts, current_markdown);
    }

    // ---- Mode-aware current content hash ----
    let content_hash = if json_only {
        match current_json {
            Some(j) => snapshot::hash_json(j),
            None => snapshot::hash_str(""),
        }
    } else {
        snapshot::hash_markdown(current_markdown)
    };

    // ---- Build the current snapshot to persist as next baseline ----
    let current_snapshot = ChangeTrackingSnapshot {
        markdown: if has_git {
            Some(current_markdown.to_string())
        } else {
            None
        },
        json: if has_json {
            current_json.cloned()
        } else {
            None
        },
        content_hash: content_hash.clone(),
        captured_at: None,
    };

    // ---- First observation: no baseline to diff against ----
    let Some(previous) = &opts.previous else {
        return ChangeTrackingResult {
            status: ChangeStatus::Changed,
            first_observation: true,
            content_hash,
            snapshot: Some(current_snapshot),
            diff: None,
            judgment: None,
            tag: opts.tag.clone(),
            truncated: false,
        };
    };

    // ---- Determine per-surface change ----
    let prev_md_norm = previous
        .markdown
        .as_deref()
        .map(snapshot::normalize_markdown);
    let cur_md_norm = snapshot::normalize_markdown(current_markdown);
    let markdown_changed = has_git
        && prev_md_norm
            .as_deref()
            .map(|p| p != cur_md_norm)
            .unwrap_or(true);

    let empty_json = Value::Null;
    let prev_json = previous.json.as_ref().unwrap_or(&empty_json);
    let cur_json_val = current_json.unwrap_or(&empty_json);
    let json_changed = has_json && json_diff::changed(prev_json, cur_json_val);

    let changed = (has_git && markdown_changed) || (has_json && json_changed);

    if !changed {
        return ChangeTrackingResult {
            status: ChangeStatus::Same,
            first_observation: false,
            content_hash,
            snapshot: Some(current_snapshot),
            diff: None,
            judgment: None,
            tag: opts.tag.clone(),
            truncated: false,
        };
    }

    // ---- Build the diff envelope ----
    let mut text: Option<String> = None;
    let mut ast_value: Option<Value> = None;
    let mut truncated = false;

    if has_git {
        let g = git_diff::compute(
            prev_md_norm.as_deref().unwrap_or(""),
            &cur_md_norm,
            limits.max_diff_changes,
        );
        truncated = g.ast.truncated;
        text = Some(g.text);
        // The AST occupies diff.json ONLY in gitDiff-only mode. In mixed mode
        // the per-field json diff takes diff.json instead (Firecrawl parity).
        if !has_json {
            ast_value = Some(serde_json::to_value(&g.ast).unwrap_or(Value::Null));
        }
    }

    let json_value: Option<Value> = if has_json {
        Some(json_diff::compute(prev_json, cur_json_val))
    } else {
        None
    };

    // diff.json: per-field map (json/mixed) wins; else the AST (gitDiff-only).
    let diff_json = json_value.or(ast_value);
    let diff = ChangeDiff {
        text,
        json: diff_json,
    };

    ChangeTrackingResult {
        status: ChangeStatus::Changed,
        first_observation: false,
        content_hash,
        snapshot: Some(current_snapshot),
        diff: Some(diff),
        judgment: None,
        tag: opts.tag.clone(),
        truncated,
    }
}

/// Binary / non-text content path: hash the extracted text for same/changed,
/// emit no diff. The orchestration layer never judges these pages.
fn binary_result(opts: &ChangeTrackingOptions, current_text: &str) -> ChangeTrackingResult {
    let content_hash = snapshot::hash_str(current_text);
    let snapshot = ChangeTrackingSnapshot {
        markdown: None,
        json: None,
        content_hash: content_hash.clone(),
        captured_at: None,
    };
    match &opts.previous {
        None => ChangeTrackingResult {
            status: ChangeStatus::Changed,
            first_observation: true,
            content_hash,
            snapshot: Some(snapshot),
            diff: None,
            judgment: None,
            tag: opts.tag.clone(),
            truncated: false,
        },
        Some(prev) => {
            let status = if prev.content_hash == content_hash {
                ChangeStatus::Same
            } else {
                ChangeStatus::Changed
            };
            ChangeTrackingResult {
                status,
                first_observation: false,
                content_hash,
                snapshot: Some(snapshot),
                diff: None,
                judgment: None,
                tag: opts.tag.clone(),
                truncated: false,
            }
        }
    }
}

/// Whether a content type should be treated as diffable text. `None` => assume
/// text (the common HTML→markdown case). Binary types (PDF, images, octet
/// stream) are hashed by extracted text only.
fn is_text(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else {
        return true;
    };
    let ct = ct.to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("html")
        || ct.contains("markdown")
        || ct.contains("javascript")
        || ct.contains("csv")
        || ct.contains("yaml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crw_core::types::ChangeTrackingMode;
    use serde_json::json;

    fn opts(
        modes: Vec<ChangeTrackingMode>,
        previous: Option<ChangeTrackingSnapshot>,
    ) -> ChangeTrackingOptions {
        ChangeTrackingOptions {
            modes,
            schema: None,
            prompt: None,
            previous,
            tag: None,
            content_type: None,
        }
    }

    fn snap_md(md: &str) -> ChangeTrackingSnapshot {
        ChangeTrackingSnapshot {
            markdown: Some(md.to_string()),
            json: None,
            content_hash: snapshot::hash_markdown(md),
            captured_at: None,
        }
    }

    #[test]
    fn first_observation_no_previous() {
        let r = compute_change_tracking(
            &opts(vec![ChangeTrackingMode::GitDiff], None),
            "# Hi",
            None,
            None,
        );
        assert!(r.first_observation);
        assert_eq!(r.status, ChangeStatus::Changed);
        assert!(r.diff.is_none());
        assert!(r.snapshot.is_some());
    }

    #[test]
    fn identical_markdown_is_same() {
        let o = opts(
            vec![ChangeTrackingMode::GitDiff],
            Some(snap_md("# Hi\n\nbody")),
        );
        let r = compute_change_tracking(&o, "# Hi\n\nbody", None, None);
        assert_eq!(r.status, ChangeStatus::Same);
        assert!(r.diff.is_none());
    }

    #[test]
    fn whitespace_only_change_is_same() {
        let o = opts(
            vec![ChangeTrackingMode::GitDiff],
            Some(snap_md("# Hi\n\nbody")),
        );
        let r = compute_change_tracking(&o, "# Hi   \n\n\n\nbody  \n", None, None);
        assert_eq!(r.status, ChangeStatus::Same);
    }

    #[test]
    fn markdown_change_emits_text_and_ast_in_git_mode() {
        let o = opts(
            vec![ChangeTrackingMode::GitDiff],
            Some(snap_md("Starter $19")),
        );
        let r = compute_change_tracking(&o, "Starter $24", None, None);
        assert_eq!(r.status, ChangeStatus::Changed);
        let diff = r.diff.unwrap();
        assert!(diff.text.unwrap().contains("+Starter $24"));
        // gitDiff-only => diff.json holds the AST (has a `files` array)
        assert!(diff.json.unwrap().get("files").is_some());
    }

    #[test]
    fn json_mode_per_field_diff() {
        let prev = ChangeTrackingSnapshot {
            markdown: None,
            json: Some(json!({"price": "$19"})),
            content_hash: snapshot::hash_json(&json!({"price": "$19"})),
            captured_at: None,
        };
        let o = opts(vec![ChangeTrackingMode::Json], Some(prev));
        let cur = json!({"price": "$24"});
        let r = compute_change_tracking(&o, "ignored markdown", Some(&cur), None);
        assert_eq!(r.status, ChangeStatus::Changed);
        let diff = r.diff.unwrap();
        assert!(diff.text.is_none());
        assert_eq!(
            diff.json.unwrap()["price"],
            json!({"previous": "$19", "current": "$24"})
        );
    }

    #[test]
    fn json_mode_same_when_tracked_fields_unchanged_even_if_markdown_differs() {
        let prev = ChangeTrackingSnapshot {
            markdown: None,
            json: Some(json!({"price": "$19"})),
            content_hash: snapshot::hash_json(&json!({"price": "$19"})),
            captured_at: None,
        };
        let o = opts(vec![ChangeTrackingMode::Json], Some(prev));
        let cur = json!({"price": "$19"});
        let r = compute_change_tracking(&o, "totally different markdown", Some(&cur), None);
        assert_eq!(r.status, ChangeStatus::Same);
    }

    #[test]
    fn mixed_mode_either_surface_changes() {
        let prev = ChangeTrackingSnapshot {
            markdown: Some("Starter $19".into()),
            json: Some(json!({"price": "$19"})),
            content_hash: snapshot::hash_markdown("Starter $19"),
            captured_at: None,
        };
        let o = opts(
            vec![ChangeTrackingMode::Json, ChangeTrackingMode::GitDiff],
            Some(prev),
        );
        let cur = json!({"price": "$24"});
        let r = compute_change_tracking(&o, "Starter $24", Some(&cur), None);
        assert_eq!(r.status, ChangeStatus::Changed);
        let diff = r.diff.unwrap();
        // mixed: text present AND diff.json is the per-field map (not the AST)
        assert!(diff.text.is_some());
        assert_eq!(
            diff.json.unwrap()["price"],
            json!({"previous": "$19", "current": "$24"})
        );
    }

    #[test]
    fn binary_content_hashes_no_diff() {
        let prev = ChangeTrackingSnapshot {
            markdown: None,
            json: None,
            content_hash: snapshot::hash_str("old pdf text"),
            captured_at: None,
        };
        let o = ChangeTrackingOptions {
            modes: vec![ChangeTrackingMode::GitDiff],
            content_type: Some("application/pdf".into()),
            ..opts(vec![ChangeTrackingMode::GitDiff], Some(prev))
        };
        let r = compute_change_tracking(&o, "new pdf text", None, Some("application/pdf"));
        assert_eq!(r.status, ChangeStatus::Changed);
        assert!(r.diff.is_none());
    }
}
