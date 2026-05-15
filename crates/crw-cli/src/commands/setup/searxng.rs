//! SearXNG Docker setup for web search.

use crate::commands::setup::docker;
use crate::commands::setup::shell;
use crate::commands::setup::ui;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::time::Duration;

pub const SEARXNG_IMAGE: &str = "searxng/searxng:latest";
pub const SEARXNG_CONTAINER_NAME: &str = "searxng";
pub const SEARXNG_DEFAULT_PORT: u16 = 8080;

const CRW_CONFIG_SUBDIR: &str = ".config/crw";
const SEARXNG_SETTINGS_FILENAME: &str = "searxng-settings.yml";
const SEARXNG_SETTINGS_MOUNT_TARGET: &str = "/etc/searxng/settings.yml:ro";

/// SearXNG installation status.
#[derive(Debug)]
pub enum SearxngStatus {
    /// Container running and healthy.
    Running { url: String },
    /// Container exists but stopped.
    Stopped,
    /// Container doesn't exist.
    NotInstalled,
}

/// Check SearXNG container status.
pub fn check_status() -> SearxngStatus {
    if docker::container_running(SEARXNG_CONTAINER_NAME) {
        SearxngStatus::Running {
            url: format!("http://localhost:{}", SEARXNG_DEFAULT_PORT),
        }
    } else if docker::container_exists(SEARXNG_CONTAINER_NAME) {
        SearxngStatus::Stopped
    } else {
        SearxngStatus::NotInstalled
    }
}

/// Pull SearXNG Docker image with progress indicator.
pub async fn pull_image() -> Result<(), String> {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("    {spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Pulling SearXNG image...");
    pb.enable_steady_tick(Duration::from_millis(100));

    // Run docker pull in a blocking task
    let result = tokio::task::spawn_blocking(|| docker::pull_image(SEARXNG_IMAGE)).await;

    pb.finish_and_clear();

    match result {
        Ok(Ok(())) => {
            ui::print_success("SearXNG image pulled");
            Ok(())
        }
        Ok(Err(e)) => Err(format!("Failed to pull image: {}", e)),
        Err(e) => Err(format!("Task error: {}", e)),
    }
}

/// Start or create SearXNG container.
pub async fn start_container() -> Result<String, String> {
    let status = check_status();

    match status {
        SearxngStatus::Running { url } => {
            ui::print_success(&format!("SearXNG already running at {}", url));
            return Ok(url);
        }
        SearxngStatus::Stopped => {
            ui::print_info("Starting existing SearXNG container...");
            docker::start_container(SEARXNG_CONTAINER_NAME)
                .map_err(|e| format!("Failed to start container: {}", e))?;
        }
        SearxngStatus::NotInstalled => {
            ui::print_info("Creating SearXNG container...");
            create_container()?;
        }
    }

    // Wait for container to be healthy
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("    {spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Waiting for SearXNG to be ready...");
    pb.enable_steady_tick(Duration::from_millis(100));

    let result = wait_for_ready(30).await;
    pb.finish_and_clear();

    result?;

    let url = format!("http://localhost:{}", SEARXNG_DEFAULT_PORT);
    ui::print_success(&format!("SearXNG running at {}", url));
    Ok(url)
}

/// Create a new SearXNG container.
fn create_container() -> Result<String, String> {
    // Check if container already exists
    if docker::container_exists(SEARXNG_CONTAINER_NAME) {
        if docker::container_running(SEARXNG_CONTAINER_NAME) {
            return Err(format!(
                "Container '{}' is already running. Stop it first with: docker stop {}",
                SEARXNG_CONTAINER_NAME, SEARXNG_CONTAINER_NAME
            ));
        }
        // Container exists but stopped - remove it
        docker::remove_container(SEARXNG_CONTAINER_NAME)?;
    }

    // Create settings file to enable JSON API
    let settings_path = create_settings_file()?;

    let container_id = docker::run_container(
        SEARXNG_CONTAINER_NAME,
        SEARXNG_IMAGE,
        Some((&SEARXNG_DEFAULT_PORT.to_string(), "8080")),
        &[
            // SearXNG environment settings
            (
                "SEARXNG_BASE_URL",
                &format!("http://localhost:{}/", SEARXNG_DEFAULT_PORT),
            ),
        ],
        &[
            "--restart",
            "unless-stopped",
            // Mount settings file to enable JSON format
            "-v",
            &format!(
                "{}:{}",
                settings_path.display(),
                SEARXNG_SETTINGS_MOUNT_TARGET
            ),
            // Resource limits for security
            "--memory",
            "512m",
            "--cpus",
            "1.0",
        ],
    )?;

    Ok(container_id)
}

/// Resolve the SearXNG settings file path under the user's config directory.
fn settings_file_path() -> Result<PathBuf, String> {
    let home = shell::home_dir().ok_or("Could not determine home directory")?;
    Ok(home.join(CRW_CONFIG_SUBDIR).join(SEARXNG_SETTINGS_FILENAME))
}

/// Generate a 256-bit secret key encoded as 64 hex chars.
///
/// Uses `rand::random`, which the `rand` crate documents as suitable for
/// cryptographic use (`ThreadRng` is CSPRNG-backed).
fn generate_secret() -> String {
    (0..32)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

/// Build the SearXNG settings YAML for the given secret.
///
/// `use_default_settings: true` merges these overrides on top of the image's defaults.
/// `limiter: false` is safe for our localhost-only bind; if the port is ever exposed
/// publicly, re-enable it.
fn build_settings_yaml(secret: &str) -> String {
    format!(
        r#"use_default_settings: true
server:
  # Random per-install secret; preserved across `crw setup` re-runs.
  secret_key: "{}"
  # Safe to disable because the container binds to localhost only.
  limiter: false
search:
  formats:
    - html
    - json
"#,
        secret
    )
}

/// Set restrictive (0600) permissions on a file containing secrets.
#[cfg(unix)]
fn set_secret_file_perms(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .map_err(|e| format!("Failed to chmod 600 on {}: {}", path.display(), e))
}

#[cfg(not(unix))]
fn set_secret_file_perms(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

/// Create the parent config dir with owner-only (0700) permissions on Unix.
///
/// Without this, `create_dir_all` honors only the umask — on a permissive
/// umask the directory ends up world-listable, even though the file inside
/// is `0600`. The dir's existence alone leaks that this user has a secret.
#[cfg(unix)]
fn ensure_secure_config_dir(dir: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::PermissionsExt;

    if dir.exists() {
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir, perms)
            .map_err(|e| format!("Failed to chmod 700 on {}: {}", dir.display(), e))?;
    } else {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_secure_config_dir(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create config dir: {}", e))
}

/// Refuse to operate on a path that is a symlink. We treat the settings file
/// as security-sensitive, so we never read/write through a symlink we don't
/// control — an attacker who can plant a symlink in `~/.config/crw/` could
/// otherwise redirect our 0600 write to a file we shouldn't be touching.
fn reject_symlink(path: &std::path::Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "Refusing to use settings file: {} is a symlink. Remove it and re-run.",
            path.display()
        )),
        _ => Ok(()),
    }
}

/// Atomic 0600 write that refuses to follow symlinks.
///
/// `O_NOFOLLOW` closes the TOCTOU window between our `reject_symlink` check
/// and this `open`: even if an attacker swaps `path` to a symlink in between,
/// `open(2)` will fail with `ELOOP` instead of redirecting our write.
#[cfg(unix)]
fn write_secret_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    const O_NOFOLLOW: i32 = libc_nofollow();

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;
    f.write_all(contents.as_bytes())
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| format!("Failed to write settings: {}", e))
}

/// O_NOFOLLOW constant. Same value on Linux and macOS/BSD (octal 0400000 vs
/// 0x100), but we define per-platform to be explicit.
#[cfg(all(unix, target_os = "linux"))]
const fn libc_nofollow() -> i32 {
    0o400000
}
#[cfg(all(unix, not(target_os = "linux")))]
const fn libc_nofollow() -> i32 {
    0x0100
}

/// Read a file refusing to follow symlinks. Same TOCTOU-closing rationale as
/// [`write_secret_file`].
#[cfg(unix)]
fn read_secret_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_nofollow())
        .open(path)?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;
    Ok(buf)
}

#[cfg(not(unix))]
fn read_secret_file(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// Check whether existing YAML contains a non-comment line we care about.
///
/// `contains("- json")` is too loose — `# - json` would pass. We require the
/// substring to appear on a line whose first non-whitespace character is not
/// `#`. This rejects commented-out config without pulling in a full YAML parser.
fn yaml_has_uncommented(yaml: &str, needle: &str) -> bool {
    yaml.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#') && trimmed.contains(needle)
    })
}

/// Decide whether an existing settings file is good enough to reuse. Looks for
/// an uncommented `secret_key:` *and* `formats:` block containing `- json`.
fn settings_file_is_valid(yaml: &str) -> bool {
    yaml_has_uncommented(yaml, "secret_key:")
        && yaml_has_uncommented(yaml, "formats:")
        && yaml_has_uncommented(yaml, "- json")
}

/// Ensure a SearXNG settings file exists with JSON API enabled, reusing any
/// previously-generated secret_key. Returns the path to mount into the container.
fn create_settings_file() -> Result<PathBuf, String> {
    let settings_path = settings_file_path()?;

    if let Some(parent) = settings_path.parent() {
        ensure_secure_config_dir(parent)?;
    }

    reject_symlink(&settings_path)?;

    // Idempotent: reuse the file if it has both a secret_key and JSON enabled
    // (non-comment lines). This preserves the random secret across re-runs.
    // We still chmod 600, since an older crw or hand-edit may have left it 0644.
    if settings_path.exists()
        && let Ok(existing) = read_secret_file(&settings_path)
        && settings_file_is_valid(&existing)
    {
        set_secret_file_perms(&settings_path)?;
        return Ok(settings_path);
    }

    let yaml = build_settings_yaml(&generate_secret());
    write_secret_file(&settings_path, &yaml)?;
    // Defensive: write_secret_file already creates with 0600 on Unix, but if a
    // file pre-existed with looser perms (and is being overwritten) some
    // implementations preserve the inode's mode. Re-chmod to be sure.
    set_secret_file_perms(&settings_path)?;

    Ok(settings_path)
}

/// Wait for SearXNG to be ready AND verify the JSON API responds.
///
/// Probing `/` only proves the HTTP listener is up. We additionally call
/// `/search?q=test&format=json` to confirm the mounted settings file actually
/// took effect — otherwise a misconfigured settings file silently degrades the
/// install to HTML-only and crw's search command fails at runtime.
async fn wait_for_ready(timeout_secs: u64) -> Result<(), String> {
    use std::time::Instant;
    use tokio::time::sleep;

    // Split the total budget so phase 2 always gets a fair window even if
    // phase 1 burned most of its half. Phase 1 = liveness (TCP/HTTP listener),
    // phase 2 = JSON content-type probe (verifies mounted settings).
    let phase1_budget = Duration::from_secs(timeout_secs);
    let phase2_budget = Duration::from_secs(timeout_secs.max(10));

    let base = format!("http://localhost:{}", SEARXNG_DEFAULT_PORT);
    let liveness_url = format!("{}/", base);
    let json_probe_url = format!("{}/search?q=test&format=json", base);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // Phase 1: wait for the listener to come up.
    let phase1_start = Instant::now();
    let mut listener_up = false;
    while phase1_start.elapsed() < phase1_budget {
        match client.get(&liveness_url).send().await {
            Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
                listener_up = true;
                break;
            }
            _ => {
                sleep(Duration::from_millis(500)).await;
            }
        }
    }

    if !listener_up {
        return Err(format!(
            "SearXNG did not become ready within {} seconds. You can check logs with: docker logs {}",
            timeout_secs, SEARXNG_CONTAINER_NAME
        ));
    }

    // Phase 2: verify JSON format is actually enabled by the mounted settings.
    // Independent timer so phase 1's slow start can't starve this check.
    let phase2_start = Instant::now();
    while phase2_start.elapsed() < phase2_budget {
        if let Ok(resp) = client.get(&json_probe_url).send().await
            && resp.status().is_success()
        {
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_lowercase();
            if ct.contains("application/json") {
                return Ok(());
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    Err(format!(
        "SearXNG is running but JSON API is not enabled. The settings file mount may have failed. Check logs with: docker logs {}",
        SEARXNG_CONTAINER_NAME
    ))
}

/// Stop SearXNG container.
#[allow(dead_code)]
pub fn stop() -> Result<(), String> {
    docker::stop_container(SEARXNG_CONTAINER_NAME)
}

/// Remove SearXNG container completely.
#[allow(dead_code)]
pub fn remove() -> Result<(), String> {
    docker::remove_container(SEARXNG_CONTAINER_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_status() {
        // This test depends on Docker being available
        let status = check_status();
        // Just make sure it doesn't panic
        match status {
            SearxngStatus::Running { .. } => {}
            SearxngStatus::Stopped => {}
            SearxngStatus::NotInstalled => {}
        }
    }

    #[test]
    fn generate_secret_is_64_hex_chars() {
        let s = generate_secret();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_secret_is_random() {
        // Two consecutive calls should differ with overwhelming probability.
        assert_ne!(generate_secret(), generate_secret());
    }

    #[test]
    fn build_settings_yaml_enables_json_format() {
        let yaml = build_settings_yaml("deadbeef");
        assert!(yaml.contains("secret_key: \"deadbeef\""));
        assert!(yaml.contains("- json"));
        assert!(yaml.contains("- html"));
        assert!(yaml.contains("use_default_settings: true"));
        assert!(yaml.contains("limiter: false"));
    }

    #[test]
    fn build_settings_yaml_keeps_secret_isolated() {
        // The secret is interpolated into a YAML string literal; verify it
        // appears exactly once and inside the quoted value.
        let yaml = build_settings_yaml("abc123");
        let matches: Vec<_> = yaml.match_indices("abc123").collect();
        assert_eq!(matches.len(), 1);
        assert!(yaml.contains("\"abc123\""));
    }

    #[test]
    fn settings_file_is_valid_accepts_real_yaml() {
        let yaml = build_settings_yaml("deadbeef");
        assert!(settings_file_is_valid(&yaml));
    }

    #[test]
    fn settings_file_is_valid_rejects_commented_lines() {
        // A previous-version file or hand-edit could leave commented config.
        // Substring match would falsely accept this; line-aware match rejects it.
        let yaml = r#"
# secret_key: "old"
# formats:
#   - json
"#;
        assert!(!settings_file_is_valid(yaml));
    }

    #[test]
    fn settings_file_is_valid_rejects_missing_json_format() {
        let yaml = r#"
server:
  secret_key: "abc"
search:
  formats:
    - html
"#;
        assert!(!settings_file_is_valid(yaml));
    }

    #[test]
    fn settings_file_is_valid_rejects_missing_secret_key() {
        let yaml = r#"
search:
  formats:
    - html
    - json
"#;
        assert!(!settings_file_is_valid(yaml));
    }

    #[test]
    fn yaml_has_uncommented_skips_pound_lines() {
        assert!(yaml_has_uncommented("foo: bar", "foo:"));
        assert!(!yaml_has_uncommented("# foo: bar", "foo:"));
        assert!(!yaml_has_uncommented("   # foo: bar", "foo:"));
        // Inline comment doesn't matter — the line is still uncommented.
        assert!(yaml_has_uncommented("foo: bar  # comment", "foo:"));
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_file_creates_with_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("crw-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.yml");
        write_secret_file(&path, "hello").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_secure_config_dir_chmods_existing_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("crw-test-dir-{}", std::process::id()));
        // Create with a loose mode first.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_secure_config_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn reject_symlink_blocks_symlinked_path() {
        use std::os::unix::fs;
        let dir = std::env::temp_dir().join(format!("crw-test-sym-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real.txt");
        std::fs::write(&target, "x").unwrap();
        let link = dir.join("link.txt");
        fs::symlink(&target, &link).unwrap();
        assert!(reject_symlink(&link).is_err());
        assert!(reject_symlink(&target).is_ok());
        // Non-existent path is fine (we'll create it ourselves).
        assert!(reject_symlink(&dir.join("nope")).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
