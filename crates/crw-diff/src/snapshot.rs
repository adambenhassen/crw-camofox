//! Markdown normalization + content hashing. Single source of truth for the
//! `content_hash` so cosmetic churn (trailing whitespace, blank-line runs,
//! CRLF) never flips a page from `same` to `changed`.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Normalize markdown before hashing/diffing:
/// - normalize CRLF / CR to LF
/// - strip trailing whitespace on every line
/// - collapse runs of 3+ blank lines to a single blank line
/// - trim leading/trailing blank lines
///
/// Diffing operates on the normalized form so the unified diff and AST never
/// report whitespace-only noise.
pub fn normalize_markdown(input: &str) -> String {
    let unified = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut out_lines: Vec<&str> = Vec::new();
    let mut blank_run = 0usize;
    for raw in unified.split('\n') {
        let line = raw.trim_end();
        if line.is_empty() {
            blank_run += 1;
            // keep at most one blank line in a run
            if blank_run <= 1 {
                out_lines.push("");
            }
        } else {
            blank_run = 0;
            out_lines.push(line);
        }
    }
    // trim leading/trailing blank lines
    while out_lines.first() == Some(&"") {
        out_lines.remove(0);
    }
    while out_lines.last() == Some(&"") {
        out_lines.pop();
    }
    out_lines.join("\n")
}

/// Hex SHA-256 of a string.
pub fn hash_str(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

/// Hex SHA-256 of the normalized markdown.
pub fn hash_markdown(markdown: &str) -> String {
    hash_str(&normalize_markdown(markdown))
}

/// Hex SHA-256 of a canonicalized JSON value (object keys sorted recursively),
/// so logically-equal extractions with different key ordering hash equal.
pub fn hash_json(value: &Value) -> String {
    hash_str(&canonical_json_string(value))
}

/// Serialize a JSON value with object keys sorted recursively. Deterministic
/// regardless of input key order.
pub fn canonical_json_string(value: &Value) -> String {
    let canonical = canonicalize(value);
    serde_json::to_string(&canonical).unwrap_or_default()
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::with_capacity(map.len());
            for k in keys {
                out.insert(k.clone(), canonicalize(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_blank_runs_and_trailing_ws() {
        let input = "# Title  \n\n\n\nbody   \n\n";
        assert_eq!(normalize_markdown(input), "# Title\n\nbody");
    }

    #[test]
    fn normalize_handles_crlf() {
        assert_eq!(normalize_markdown("a\r\nb\r\n"), "a\nb");
    }

    #[test]
    fn whitespace_only_change_hashes_equal() {
        let a = "# Hello\n\nworld";
        let b = "# Hello   \n\n\n\nworld  \n";
        assert_eq!(hash_markdown(a), hash_markdown(b));
    }

    #[test]
    fn json_key_order_hashes_equal() {
        let a: Value = serde_json::json!({"a": 1, "b": [1, 2]});
        let b: Value = serde_json::json!({"b": [1, 2], "a": 1});
        assert_eq!(hash_json(&a), hash_json(&b));
    }
}
