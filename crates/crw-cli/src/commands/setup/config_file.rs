//! Atomic writer for the per-user config file at `~/.config/crw/config.toml`.
//!
//! Setup populates a [`UserConfig`] in memory, merges it with whatever is
//! already on disk (so a re-run that only configures the LLM doesn't blow
//! away the SearXNG URL from a previous run), and writes the result via a
//! tmpfile + rename to keep the on-disk file atomic and 0600-owned.
//!
//! Hardening mirrors the SearXNG settings file: parent dir forced to 0700,
//! symlink targets refused, write goes through `O_NOFOLLOW` so a swap-in
//! between our check and our open cannot redirect the write elsewhere.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const USER_CONFIG_FILENAME: &str = "config.toml";
const USER_CONFIG_SUBDIR: &str = ".config/crw";

/// Subset of `crw-core::AppConfig` that `crw setup` knows how to populate.
///
/// We deliberately don't re-export the upstream struct — its tree is huge
/// and most fields belong to operators (rate limits, escalation policies,
/// etc). Setup is for end-user credentials and URLs only. Anything missing
/// here can still be edited by hand or set via env var.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction: Option<ExtractionSection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub searxng_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmSection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure_api_version: Option<String>,
}

/// Resolve the path of the per-user config file. Honors
/// `$CRW_USER_CONFIG_DIR` so tests don't have to monkey-patch `$HOME`.
pub fn user_config_path() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("CRW_USER_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join(USER_CONFIG_FILENAME));
    }
    let home = std::env::var_os("HOME").ok_or("Could not determine home directory")?;
    Ok(PathBuf::from(home)
        .join(USER_CONFIG_SUBDIR)
        .join(USER_CONFIG_FILENAME))
}

/// Read the existing config file if present. Missing file → empty config.
/// Malformed TOML returns an error rather than silently dropping settings.
pub fn read_user_config(path: &Path) -> Result<UserConfig, String> {
    if !path.exists() {
        return Ok(UserConfig::default());
    }
    let raw =
        read_no_follow(path).map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    toml::from_str(&raw).map_err(|e| format!("Failed to parse {}: {}", path.display(), e))
}

/// Merge `update` into `base` field by field. Any `Some` in `update` wins;
/// `None` keeps the existing value. This is what makes the setup wizard
/// idempotent across sub-flows: configuring just the LLM later doesn't
/// blank out the SearXNG URL from the first run.
pub fn merge_config(base: UserConfig, update: UserConfig) -> UserConfig {
    UserConfig {
        client: merge_client(base.client, update.client),
        search: merge_search(base.search, update.search),
        extraction: merge_extraction(base.extraction, update.extraction),
    }
}

fn merge_client(
    base: Option<ClientSection>,
    update: Option<ClientSection>,
) -> Option<ClientSection> {
    match (base, update) {
        (None, u) => u,
        (b, None) => b,
        (Some(b), Some(u)) => Some(ClientSection {
            api_url: u.api_url.or(b.api_url),
            api_key: u.api_key.or(b.api_key),
        }),
    }
}

fn merge_search(
    base: Option<SearchSection>,
    update: Option<SearchSection>,
) -> Option<SearchSection> {
    match (base, update) {
        (None, u) => u,
        (b, None) => b,
        (Some(b), Some(u)) => Some(SearchSection {
            searxng_url: u.searxng_url.or(b.searxng_url),
        }),
    }
}

fn merge_extraction(
    base: Option<ExtractionSection>,
    update: Option<ExtractionSection>,
) -> Option<ExtractionSection> {
    match (base, update) {
        (None, u) => u,
        (b, None) => b,
        (Some(b), Some(u)) => Some(ExtractionSection {
            llm: merge_llm(b.llm, u.llm),
        }),
    }
}

fn merge_llm(base: Option<LlmSection>, update: Option<LlmSection>) -> Option<LlmSection> {
    match (base, update) {
        (None, u) => u,
        (b, None) => b,
        (Some(b), Some(u)) => Some(LlmSection {
            provider: u.provider.or(b.provider),
            api_key: u.api_key.or(b.api_key),
            model: u.model.or(b.model),
            base_url: u.base_url.or(b.base_url),
            azure_api_version: u.azure_api_version.or(b.azure_api_version),
        }),
    }
}

/// Write the merged config to disk atomically.
///
/// 1. Ensure parent dir exists at 0700.
/// 2. Refuse to follow a symlink at the destination.
/// 3. Read existing file, merge with `update`, serialize.
/// 4. Write to `config.toml.tmp` with 0600 + O_NOFOLLOW.
/// 5. Rename onto `config.toml` (atomic on POSIX).
///
/// Returns the final path so callers can show it to the user.
pub fn write_user_config(update: UserConfig) -> Result<PathBuf, String> {
    let path = user_config_path()?;

    if let Some(parent) = path.parent() {
        ensure_secure_dir(parent)?;
    }
    reject_symlink(&path)?;

    let merged = merge_config(read_user_config(&path)?, update);
    let body = render(&merged);

    let tmp = path.with_extension("toml.tmp");
    // The tmp path is in the same dir, so the same symlink-rejection logic applies.
    reject_symlink(&tmp)?;
    write_secret_file(&tmp, &body)?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp.display(),
            path.display(),
            e
        )
    })?;
    set_secret_file_perms(&path)?;

    Ok(path)
}

/// Render the config with a header comment users can see when they `cat` it.
fn render(cfg: &UserConfig) -> String {
    let mut out = String::new();
    out.push_str("# Generated by `crw setup`. Re-run `crw setup` to update.\n");
    out.push_str("# Override individual values with CRW_* env vars (highest precedence).\n");
    out.push_str("# Hand-edits are preserved on re-run for keys you don't change.\n\n");
    // toml::to_string_pretty omits empty Option fields thanks to
    // skip_serializing_if on each Option field.
    let body = toml::to_string_pretty(cfg).expect("UserConfig should serialize");
    out.push_str(&body);
    out
}

// ---- Filesystem helpers ----------------------------------------------------
// Same hardening pattern as searxng.rs: enforce 0700 on the parent dir, refuse
// to follow symlinks, open the file with O_NOFOLLOW + mode 0o600 atomically.

#[cfg(unix)]
fn ensure_secure_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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
fn ensure_secure_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {}", dir.display(), e))
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "Refusing to use config file: {} is a symlink. Remove it and re-run.",
            path.display()
        )),
        _ => Ok(()),
    }
}

#[cfg(all(unix, target_os = "linux"))]
const fn nofollow_flag() -> i32 {
    0o400000
}
#[cfg(all(unix, not(target_os = "linux")))]
const fn nofollow_flag() -> i32 {
    0x0100
}

#[cfg(unix)]
fn write_secret_file(path: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(nofollow_flag())
        .open(path)
        .map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;
    f.write_all(contents.as_bytes())
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

#[cfg(unix)]
fn read_no_follow(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_flag())
        .open(path)?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;
    Ok(buf)
}

#[cfg(not(unix))]
fn read_no_follow(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

#[cfg(unix)]
fn set_secret_file_perms(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .map_err(|e| format!("Failed to chmod 600 on {}: {}", path.display(), e))
}

#[cfg(not(unix))]
fn set_secret_file_perms(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    // Tests mutate CRW_USER_CONFIG_DIR (process-global env), so they must run
    // serially even though `cargo test` is multi-threaded by default.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn isolated_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crw-cfgfile-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("CRW_USER_CONFIG_DIR", &dir);
        }
        dir
    }

    fn cleanup(dir: &Path) {
        unsafe {
            std::env::remove_var("CRW_USER_CONFIG_DIR");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn write_then_read_roundtrip() {
        let _g = env_lock();
        let dir = isolated_dir("roundtrip");
        let cfg = UserConfig {
            client: Some(ClientSection {
                api_url: Some("https://api.example.com".into()),
                api_key: Some("test-key".into()),
            }),
            search: Some(SearchSection {
                searxng_url: Some("http://localhost:8080".into()),
            }),
            extraction: None,
        };
        let path = write_user_config(cfg.clone()).unwrap();
        let on_disk = read_user_config(&path).unwrap();
        assert_eq!(
            on_disk.client.unwrap().api_url.as_deref(),
            Some("https://api.example.com")
        );
        assert_eq!(
            on_disk.search.unwrap().searxng_url.as_deref(),
            Some("http://localhost:8080")
        );
        cleanup(&dir);
    }

    #[test]
    fn second_write_merges_not_overwrites() {
        // Idempotency contract: writing just the LLM section must keep
        // an earlier search.searxng_url intact.
        let _g = env_lock();
        let dir = isolated_dir("merge");

        let first = UserConfig {
            search: Some(SearchSection {
                searxng_url: Some("http://localhost:8080".into()),
            }),
            ..Default::default()
        };
        write_user_config(first).unwrap();

        let second = UserConfig {
            extraction: Some(ExtractionSection {
                llm: Some(LlmSection {
                    provider: Some("deepseek".into()),
                    api_key: Some("sk-1".into()),
                    model: Some("deepseek-chat".into()),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        };
        let path = write_user_config(second).unwrap();

        let merged = read_user_config(&path).unwrap();
        assert_eq!(
            merged.search.unwrap().searxng_url.as_deref(),
            Some("http://localhost:8080"),
            "first run's searxng_url must survive second write"
        );
        let llm = merged.extraction.unwrap().llm.unwrap();
        assert_eq!(llm.provider.as_deref(), Some("deepseek"));
        cleanup(&dir);
    }

    #[test]
    fn rewrite_replaces_changed_value() {
        // If the user picks a new LLM provider, the new value must win —
        // merge_llm uses `update.or(base)`, so update wins when both are Some.
        let _g = env_lock();
        let dir = isolated_dir("replace");
        write_user_config(UserConfig {
            extraction: Some(ExtractionSection {
                llm: Some(LlmSection {
                    provider: Some("anthropic".into()),
                    api_key: Some("old-key".into()),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        })
        .unwrap();
        let path = write_user_config(UserConfig {
            extraction: Some(ExtractionSection {
                llm: Some(LlmSection {
                    provider: Some("deepseek".into()),
                    api_key: Some("new-key".into()),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        })
        .unwrap();
        let llm = read_user_config(&path)
            .unwrap()
            .extraction
            .unwrap()
            .llm
            .unwrap();
        assert_eq!(llm.provider.as_deref(), Some("deepseek"));
        assert_eq!(llm.api_key.as_deref(), Some("new-key"));
        cleanup(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_0600_and_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let _g = env_lock();
        let dir = isolated_dir("perms");
        let path = write_user_config(UserConfig {
            client: Some(ClientSection {
                api_url: Some("u".into()),
                api_key: None,
            }),
            ..Default::default()
        })
        .unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
        cleanup(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_write_through_symlink() {
        let _g = env_lock();
        let dir = isolated_dir("sym");
        let real_target = std::env::temp_dir().join(format!(
            "crw-sym-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&real_target, "untouched").unwrap();
        let cfg_path = dir.join("config.toml");
        std::os::unix::fs::symlink(&real_target, &cfg_path).unwrap();
        let err = write_user_config(UserConfig::default()).unwrap_err();
        assert!(err.contains("symlink"), "got: {err}");
        // Real target must not have been touched.
        assert_eq!(std::fs::read_to_string(&real_target).unwrap(), "untouched");
        std::fs::remove_file(&real_target).ok();
        cleanup(&dir);
    }

    #[test]
    fn render_includes_header_comments() {
        let s = render(&UserConfig::default());
        assert!(s.starts_with("# Generated by `crw setup`"));
        assert!(s.contains("Override individual values"));
    }

    #[test]
    fn render_omits_empty_sections() {
        // Default UserConfig has all None — output should be just the header
        // plus an empty body, with no `[client]` or `[search]` headers.
        let s = render(&UserConfig::default());
        assert!(!s.contains("[client]"));
        assert!(!s.contains("[search]"));
        assert!(!s.contains("[extraction"));
    }
}
