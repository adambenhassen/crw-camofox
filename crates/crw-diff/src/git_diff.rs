//! Git-diff (markdown) surface: a unified text diff plus a parse-diff-style
//! AST, BOTH derived from the same `similar` op stream so they can never
//! disagree. There is no `parse-diff` crate in Rust; the AST is synthesized
//! directly from `similar`'s `DiffOp`/`ChangeTag` stream.

use crw_core::types::{DiffAst, DiffChange, DiffChunk, DiffFile};
use similar::{ChangeTag, TextDiff};

const CONTEXT_RADIUS: usize = 3;

/// Output of a git-diff computation: the unified `text` surface and the typed
/// AST. Both come from one op stream over the same normalized inputs.
pub struct GitDiff {
    pub text: String,
    pub ast: DiffAst,
}

/// Compute the unified text + AST between two already-normalized markdown
/// strings. `max_changes` caps the number of AST change-lines; on overflow the
/// AST is marked `truncated` (the full snapshot is retained by the caller, so
/// the change is recoverable). The `text` surface is always complete.
pub fn compute(previous: &str, current: &str, max_changes: usize) -> GitDiff {
    let diff = TextDiff::from_lines(previous, current);

    // Unified text surface (always complete, independent of the AST cap).
    let text = diff
        .unified_diff()
        .context_radius(CONTEXT_RADIUS)
        .header("previous", "current")
        .to_string();

    // AST surface, synthesized from the same op stream.
    let mut chunks: Vec<DiffChunk> = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let mut emitted = 0usize;
    let mut truncated = false;

    'outer: for group in diff.grouped_ops(CONTEXT_RADIUS).iter() {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_start = first.old_range().start;
        let new_start = first.new_range().start;
        let old_lines = last.old_range().end - old_start;
        let new_lines = last.new_range().end - new_start;
        let header = format!(
            "@@ -{},{} +{},{} @@",
            old_start + 1,
            old_lines,
            new_start + 1,
            new_lines
        );

        let mut changes: Vec<DiffChange> = Vec::new();
        for op in group {
            for change in diff.iter_changes(op) {
                if emitted >= max_changes {
                    truncated = true;
                    break 'outer;
                }
                let content = change.value().trim_end_matches('\n').to_string();
                let dc = match change.tag() {
                    ChangeTag::Delete => {
                        deletions += 1;
                        DiffChange {
                            change_type: "del".into(),
                            content,
                            ln: change.old_index().map(|i| i + 1),
                            ln1: None,
                            ln2: None,
                        }
                    }
                    ChangeTag::Insert => {
                        additions += 1;
                        DiffChange {
                            change_type: "add".into(),
                            content,
                            ln: change.new_index().map(|i| i + 1),
                            ln1: None,
                            ln2: None,
                        }
                    }
                    ChangeTag::Equal => DiffChange {
                        change_type: "normal".into(),
                        content,
                        ln: None,
                        ln1: change.old_index().map(|i| i + 1),
                        ln2: change.new_index().map(|i| i + 1),
                    },
                };
                emitted += 1;
                changes.push(dc);
            }
        }

        chunks.push(DiffChunk {
            content: header,
            changes,
            old_start: old_start + 1,
            old_lines,
            new_start: new_start + 1,
            new_lines,
        });
    }

    let file = DiffFile {
        from: "previous".into(),
        to: "current".into(),
        additions,
        deletions,
        chunks,
    };
    let ast = DiffAst {
        files: vec![file],
        additions,
        deletions,
        truncated,
    };

    GitDiff { text, ast }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_input_yields_empty_diff() {
        let g = compute("a\nb\nc", "a\nb\nc", 5000);
        assert_eq!(g.ast.additions, 0);
        assert_eq!(g.ast.deletions, 0);
        assert!(g.ast.files[0].chunks.is_empty());
    }

    #[test]
    fn single_line_change_counts() {
        let g = compute("# Pricing\nStarter $19", "# Pricing\nStarter $24", 5000);
        assert_eq!(g.ast.additions, 1);
        assert_eq!(g.ast.deletions, 1);
        assert!(g.text.contains("-Starter $19"));
        assert!(g.text.contains("+Starter $24"));
        // text and AST agree on counts
        let add_in_ast: usize = g.ast.files[0]
            .chunks
            .iter()
            .flat_map(|c| &c.changes)
            .filter(|c| c.change_type == "add")
            .count();
        assert_eq!(add_in_ast, g.ast.additions);
    }

    #[test]
    fn cap_marks_truncated() {
        let prev = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cur = (0..100)
            .map(|i| format!("changed {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let g = compute(&prev, &cur, 10);
        assert!(g.ast.truncated);
    }
}
