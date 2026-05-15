//! Shell detection and RC file configuration.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Supported shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Unknown,
}

impl Shell {
    /// Human-readable name for the shell.
    pub fn name(&self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Detect the current user's shell.
pub fn detect_shell() -> Shell {
    // First, check $SHELL env var
    if let Ok(shell_path) = env::var("SHELL") {
        let basename = std::path::Path::new(&shell_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        return match basename {
            "bash" => Shell::Bash,
            "zsh" => Shell::Zsh,
            "fish" => Shell::Fish,
            _ => Shell::Unknown,
        };
    }

    // Fallback: check common shell paths
    if std::path::Path::new("/bin/zsh").exists() || std::path::Path::new("/usr/bin/zsh").exists() {
        return Shell::Zsh;
    }

    if std::path::Path::new("/bin/bash").exists() || std::path::Path::new("/usr/bin/bash").exists()
    {
        return Shell::Bash;
    }

    Shell::Unknown
}

/// Get the RC file path for a given shell.
pub fn get_rc_file(shell: Shell) -> Option<PathBuf> {
    let home = home_dir()?;

    match shell {
        Shell::Zsh => Some(home.join(".zshrc")),
        Shell::Bash => {
            // Prefer .bashrc, but use .bash_profile on macOS if .bashrc doesn't exist
            let bashrc = home.join(".bashrc");
            let bash_profile = home.join(".bash_profile");

            if bashrc.exists() {
                Some(bashrc)
            } else if bash_profile.exists() {
                Some(bash_profile)
            } else {
                // Default to .bashrc
                Some(bashrc)
            }
        }
        Shell::Fish => Some(home.join(".config").join("fish").join("config.fish")),
        Shell::Unknown => None,
    }
}

/// Get the home directory.
pub fn home_dir() -> Option<PathBuf> {
    env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// Get ~/.local/bin directory (creating if needed).
pub fn local_bin_dir() -> PathBuf {
    let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("bin")
}

/// Configuration entry to add to shell RC file.
#[derive(Clone)]
pub struct ShellConfig {
    /// Comment header for the config block.
    pub header: String,
    /// Lines to add (each line is a full shell command/export).
    pub lines: Vec<String>,
}

impl ShellConfig {
    /// Create a new shell config for CRW.
    pub fn new() -> Self {
        Self {
            header: "CRW Configuration (added by crw setup)".to_string(),
            lines: Vec::new(),
        }
    }

    /// Add an export line.
    pub fn export(&mut self, key: &str, value: &str) -> &mut Self {
        self.lines.push(format!("export {}=\"{}\"", key, value));
        self
    }

    /// Add a PATH modification.
    pub fn add_to_path(&mut self, path: &str) -> &mut Self {
        self.lines.push(format!("export PATH=\"{}:$PATH\"", path));
        self
    }

    /// Generate the shell config block as a string.
    pub fn generate(&self, shell: Shell) -> String {
        let comment_prefix = match shell {
            Shell::Fish => "#",
            _ => "#",
        };

        let mut output = String::new();
        output.push('\n');
        output.push_str(&format!("{} {}\n", comment_prefix, self.header));

        for line in &self.lines {
            // Convert to fish syntax if needed
            let converted = if shell == Shell::Fish {
                convert_to_fish(line)
            } else {
                line.clone()
            };
            output.push_str(&converted);
            output.push('\n');
        }

        output
    }

    /// Check if the config is already present in a file.
    #[allow(dead_code)]
    pub fn is_present_in(&self, content: &str) -> bool {
        // Check if ALL lines are already present (line-based matching)
        let content_lines: Vec<&str> = content.lines().map(|l| l.trim()).collect();
        self.lines.iter().all(|line| {
            let trimmed = line.trim();
            content_lines.contains(&trimmed)
        })
    }

    /// Filter out lines that are already present in content.
    /// Uses line-based matching to avoid substring false positives.
    pub fn filter_existing(&mut self, content: &str) {
        let content_lines: Vec<&str> = content.lines().map(|l| l.trim()).collect();
        self.lines.retain(|line| {
            let trimmed = line.trim();
            // Check if this exact line already exists
            !content_lines.contains(&trimmed)
        });
    }
}

/// Convert bash export syntax to fish set syntax.
fn convert_to_fish(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("export ")
        && let Some((key, value)) = rest.split_once('=')
    {
        let value = value.trim_matches('"');
        // Handle PATH specially
        if key == "PATH" && value.contains("$PATH") {
            let new_path = value.replace(":$PATH", "").replace("$PATH:", "");
            return format!("fish_add_path {}", new_path);
        }
        return format!("set -gx {} {}", key, value);
    }
    line.to_string()
}

/// Write content to a file with secure permissions (0600 on Unix).
#[cfg(unix)]
fn write_secure(path: &PathBuf, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600) // Owner read/write only
        .open(path)?;

    let mut writer = std::io::BufWriter::new(file);
    writer.write_all(content.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secure(path: &PathBuf, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}

/// Append configuration to a shell RC file (idempotent).
pub fn append_to_rc(shell: Shell, config: &ShellConfig) -> Result<PathBuf, String> {
    let rc_path =
        get_rc_file(shell).ok_or_else(|| "Could not determine RC file path".to_string())?;

    // Read existing content
    let existing = if rc_path.exists() {
        fs::read_to_string(&rc_path)
            .map_err(|e| format!("Failed to read {}: {}", rc_path.display(), e))?
    } else {
        // Create parent directories if needed
        if let Some(parent) = rc_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
        }
        String::new()
    };

    // Filter out lines that already exist (idempotent)
    let mut config = config.clone();
    config.filter_existing(&existing);

    // If all lines already exist, nothing to do
    if config.lines.is_empty() {
        return Ok(rc_path);
    }

    // Append only the new lines with secure permissions
    let new_content = format!("{}{}", existing, config.generate(shell));
    write_secure(&rc_path, &new_content)
        .map_err(|e| format!("Failed to write {}: {}", rc_path.display(), e))?;

    Ok(rc_path)
}

/// Result of `reset_rc`.
#[derive(Debug)]
pub struct ResetReport {
    pub rc_path: PathBuf,
    pub lines_removed: usize,
}

/// Strip every `# CRW Configuration (added by crw setup)` block from the
/// user's shell rc and write the cleaned file back.
///
/// A "block" is the marker line plus the *contiguous* run of lines after
/// it that look like something `crw setup` wrote: `export CRW_…`,
/// `export PATH="$HOME/.local/bin:$PATH"`, the fish equivalents, and the
/// blank line we prepend to the block. The scan stops at the first line
/// that doesn't match — we'd rather under-clean than nuke a user's own
/// export they happened to write right after our block.
///
/// Returns the count of removed lines and the path that was rewritten so
/// callers can show a summary. Returns Ok with `lines_removed: 0` if the
/// file doesn't exist or contains no markers.
pub fn reset_rc(shell: Shell) -> Result<ResetReport, String> {
    let rc_path =
        get_rc_file(shell).ok_or_else(|| "Could not determine RC file path".to_string())?;

    if !rc_path.exists() {
        return Ok(ResetReport {
            rc_path,
            lines_removed: 0,
        });
    }

    let original = fs::read_to_string(&rc_path)
        .map_err(|e| format!("Failed to read {}: {}", rc_path.display(), e))?;

    let (cleaned, removed) = strip_crw_blocks(&original);
    if removed == 0 {
        return Ok(ResetReport {
            rc_path,
            lines_removed: 0,
        });
    }

    write_secure(&rc_path, &cleaned)
        .map_err(|e| format!("Failed to write {}: {}", rc_path.display(), e))?;

    Ok(ResetReport {
        rc_path,
        lines_removed: removed,
    })
}

/// Pure string transformation extracted from `reset_rc` so the block-
/// matching logic is unit-testable without touching the filesystem.
fn strip_crw_blocks(input: &str) -> (String, usize) {
    let lines: Vec<&str> = input.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut removed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("# CRW Configuration") {
            // Drop the marker line itself.
            removed += 1;
            i += 1;
            // Drop contiguous lines that look like setup-generated exports.
            while i < lines.len() && is_setup_generated_line(lines[i]) {
                removed += 1;
                i += 1;
            }
            // Also reclaim the single trailing blank line `generate()` always
            // inserts *before* the marker — if `out` ends with a blank line
            // immediately preceding what we just stripped, drop it too so we
            // don't leave a growing stack of empty lines on repeated resets.
            if let Some(last) = out.last()
                && last.trim().is_empty()
            {
                out.pop();
                removed += 1;
            }
            continue;
        }
        out.push(line);
        i += 1;
    }
    let mut cleaned = out.join("\n");
    // Preserve the original file's trailing-newline convention, but only
    // when there's any content left to terminate — stripping every line
    // should yield an empty file, not a lone newline.
    if !cleaned.is_empty() && input.ends_with('\n') && !cleaned.ends_with('\n') {
        cleaned.push('\n');
    }
    (cleaned, removed)
}

/// Recognize the exact line shapes `ShellConfig::generate` emits, plus the
/// fish-syntax variants from `convert_to_fish`. Anything else stops the scan.
fn is_setup_generated_line(line: &str) -> bool {
    let l = line.trim_start();
    // bash/zsh
    if l.starts_with("export CRW_") {
        return true;
    }
    if l == "export PATH=\"$HOME/.local/bin:$PATH\"" {
        return true;
    }
    // fish
    if l.starts_with("set -gx CRW_") {
        return true;
    }
    if l == "fish_add_path $HOME/.local/bin" {
        return true;
    }
    false
}

/// Get the source command for applying RC file changes.
pub fn source_command(shell: Shell) -> Option<String> {
    let rc_path = get_rc_file(shell)?;
    let rc_str = rc_path.to_str()?;

    match shell {
        Shell::Fish => Some(format!("source {}", rc_str)),
        _ => Some(format!("source {}", rc_str)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_shell() {
        // This will depend on the test environment
        let shell = detect_shell();
        assert!(matches!(
            shell,
            Shell::Bash | Shell::Zsh | Shell::Fish | Shell::Unknown
        ));
    }

    #[test]
    fn test_shell_config_generate() {
        let mut config = ShellConfig::new();
        config.export("CRW_API_KEY", "test-key");
        config.add_to_path("$HOME/.local/bin");

        let output = config.generate(Shell::Bash);
        assert!(output.contains("export CRW_API_KEY=\"test-key\""));
        assert!(output.contains("export PATH=\"$HOME/.local/bin:$PATH\""));
    }

    #[test]
    fn test_convert_to_fish() {
        assert_eq!(convert_to_fish("export FOO=\"bar\""), "set -gx FOO bar");
        assert_eq!(
            convert_to_fish("export PATH=\"$HOME/.local/bin:$PATH\""),
            "fish_add_path $HOME/.local/bin"
        );
    }

    // ---- strip_crw_blocks --------------------------------------------------

    #[test]
    fn strip_removes_single_block_with_leading_blank() {
        let input = "alias g=git\n\n# CRW Configuration (added by crw setup)\nexport CRW_API_KEY=\"k\"\nexport CRW_API_URL=\"u\"\n";
        let (out, removed) = strip_crw_blocks(input);
        assert_eq!(out, "alias g=git\n");
        // marker + 2 exports + the blank line we reclaimed before the marker
        assert_eq!(removed, 4);
    }

    #[test]
    fn strip_removes_multiple_blocks_idempotent() {
        // Mirrors the user's bug: re-running setup left two stacked blocks
        // with conflicting CRW_EXTRACTION__LLM__PROVIDER values.
        let input = "\n# CRW Configuration (added by crw setup)\nexport CRW_EXTRACTION__LLM__PROVIDER=\"anthropic\"\nexport CRW_EXTRACTION__LLM__API_KEY=\"old\"\n\n# CRW Configuration (added by crw setup)\nexport CRW_EXTRACTION__LLM__PROVIDER=\"deepseek\"\nexport CRW_EXTRACTION__LLM__API_KEY=\"new\"\n";
        let (out, removed) = strip_crw_blocks(input);
        assert_eq!(out, "");
        // 2 markers + 4 exports + 2 reclaimed blank lines = 8
        assert_eq!(removed, 8);
    }

    #[test]
    fn strip_stops_at_unrelated_export() {
        // We must NOT eat the user's own non-CRW export that happens to sit
        // right after our block.
        let input = "# CRW Configuration (added by crw setup)\nexport CRW_API_KEY=\"k\"\nexport PG_HOST=\"localhost\"\n";
        let (out, _) = strip_crw_blocks(input);
        assert!(out.contains("PG_HOST"));
        assert!(!out.contains("CRW_API_KEY"));
    }

    #[test]
    fn strip_noop_when_no_markers() {
        let input = "alias g=git\nexport FOO=\"bar\"\n";
        let (out, removed) = strip_crw_blocks(input);
        assert_eq!(out, input);
        assert_eq!(removed, 0);
    }

    #[test]
    fn strip_preserves_trailing_newline_convention() {
        // No trailing newline in input -> no trailing newline in output.
        let input = "alias g=git";
        let (out, removed) = strip_crw_blocks(input);
        assert_eq!(out, "alias g=git");
        assert_eq!(removed, 0);
    }

    #[test]
    fn strip_handles_fish_lines() {
        let input = "# CRW Configuration (added by crw setup)\nset -gx CRW_API_KEY k\nfish_add_path $HOME/.local/bin\n";
        let (out, removed) = strip_crw_blocks(input);
        assert_eq!(out, "");
        assert_eq!(removed, 3);
    }
}
