//! Auto-detect and spawn a headless browser for JS rendering in embedded mode.
//!
//! Priority order:
//! 1. LightPanda binary (PATH or `~/.crw/lightpanda`, auto-downloaded if missing)
//! 2. Chrome/Chromium binary (heavier but widely available)
//! 3. LightPanda Docker container (last resort, requires Docker daemon)
//!
//! The spawned process/container is automatically cleaned up on drop.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Process-group IDs of every browser we spawned. Each native browser is
/// spawned with `process_group(0)`, making it its own group leader, so the
/// pgid equals the child PID. Group-killing the pgid reaps the browser plus
/// every grandchild (Chrome zygote/renderers, LightPanda helpers) that a
/// direct-PID `start_kill()` would miss (rust-lang/rust#115241).
///
/// This registry is the only thing robust to `process::exit`/signal — the
/// dominant leak cause — because it does not depend on `Drop` running.
static BROWSER_PGIDS: LazyLock<Mutex<HashSet<i32>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Lock the registry, recovering from a poisoned mutex. A panic in one
/// teardown path must not cascade-abort the others.
#[cfg(unix)]
fn lock_pgids() -> std::sync::MutexGuard<'static, HashSet<i32>> {
    BROWSER_PGIDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Register a freshly-spawned child's process group. Returns the pgid to
/// store on the guard, or `None` if the child already exited (no panic).
#[cfg(unix)]
fn register_child(child: &Child) -> Option<i32> {
    let pgid = child.id()? as i32;
    lock_pgids().insert(pgid);
    tracing::debug!(pgid, "registered browser process group");
    Some(pgid)
}

#[cfg(not(unix))]
fn register_child(_child: &Child) -> Option<i32> {
    None
}

/// Drop a pgid from the registry the moment its group leader is reaped, so
/// the set does not hold stale pgids across a normal browser lifetime
/// (PID-reuse mitigation).
#[cfg(unix)]
fn deregister_pgid(pgid: i32) {
    lock_pgids().remove(&pgid);
    tracing::debug!(pgid, "deregistered browser process group");
}

/// SIGKILL every still-registered browser process group. Idempotent and
/// safe to call from a signal/teardown path or `Drop`. Drains under the
/// lock then kills lock-free so a re-entrant signal cannot deadlock on the
/// registry mutex.
#[cfg(unix)]
pub fn kill_all_browsers() {
    let pgids: Vec<i32> = {
        let mut set = lock_pgids();
        set.drain().collect()
    };
    let total = pgids.len();
    let mut killed = 0usize;
    let mut already_gone = 0usize;
    for pgid in pgids {
        // SAFETY: killpg is async-signal-safe. The residual race (leader
        // reaped + pgid reused between drain and killpg) is a documented,
        // accepted rare trade-off (see plan Open Questions).
        if unsafe { libc::killpg(pgid, libc::SIGKILL) } == 0 {
            killed += 1;
        } else {
            already_gone += 1;
        }
    }
    if total > 0 {
        tracing::info!(
            registered = total,
            killed,
            already_gone,
            "kill_all_browsers: reaped browser process groups"
        );
    }
}

/// No-op on non-Unix: process groups / `killpg` are Unix-only. Browsers
/// degrade to `kill_on_drop(true)` (documented).
#[cfg(not(unix))]
pub fn kill_all_browsers() {}

/// A managed browser process or Docker container.
/// Automatically cleaned up when dropped.
pub struct ManagedBrowser {
    kind: BrowserKind,
}

enum BrowserKind {
    /// A native process (LightPanda binary or Chrome). `pgid` is the
    /// process-group id registered in `BROWSER_PGIDS` (`None` if the child
    /// had already exited at spawn time, or on non-Unix).
    Process { child: Child, pgid: Option<i32> },
    /// A Docker container, identified by its container ID.
    Docker(String),
}

impl Drop for ManagedBrowser {
    fn drop(&mut self) {
        match &mut self.kind {
            BrowserKind::Process { child, pgid } => {
                #[cfg(unix)]
                if let Some(pg) = *pgid {
                    // SAFETY: killpg is async-signal-safe. Group-kill first
                    // so Chrome zygote/renderers + LightPanda helpers die,
                    // not just the direct child PID.
                    unsafe { libc::killpg(pg, libc::SIGKILL) };
                    deregister_pgid(pg);
                }
                #[cfg(not(unix))]
                let _ = pgid;
                let _ = child.start_kill();
                // Exactly one non-blocking reap attempt — never block a
                // tokio worker (no loop, no sleep, never `wait()`). Full
                // zombie reaping for the rare long-lived-parent case is
                // offloaded to the teardown path; short-lived CLI runs are
                // reaped by the OS on process exit.
                let _ = child.try_wait();
            }
            BrowserKind::Docker(container_id) => {
                // Best-effort stop + remove. Fire-and-forget.
                let _ = std::process::Command::new("docker")
                    .args(["rm", "-f", container_id])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
        }
    }
}

/// Which renderer engine was spawned.
#[derive(Debug, Clone, Copy)]
pub enum RendererKind {
    LightPanda,
    Chrome,
}

/// Try to spawn a browser. Returns the managed handle + WS URL for CDP.
///
/// Tries in order: LightPanda native → Chrome native → LightPanda Docker.
pub async fn spawn_headless() -> Option<(ManagedBrowser, String)> {
    // 1. Try LightPanda native binary (PATH, ~/.crw/lightpanda, or auto-download).
    if let Some(result) = try_lightpanda_native().await {
        return Some(result);
    }

    // 2. Fallback to Chrome/Chromium native binary (widely available).
    if let Some(result) = try_chrome_native().await {
        return Some(result);
    }

    // 3. Last resort: LightPanda via Docker (requires Docker daemon).
    try_lightpanda_docker().await
}

/// Spawn all available browsers for a multi-renderer fallback chain.
///
/// Unlike `spawn_headless()` which returns the first browser found, this
/// function spawns every available browser so that `FallbackRenderer` can
/// try LightPanda first (fast, lightweight) and fall back to Chrome
/// (heavier but handles complex SPAs).
///
/// Docker is only tried if no native browser was found at all.
pub async fn spawn_all_headless() -> Vec<(ManagedBrowser, String, RendererKind)> {
    let mut browsers = Vec::new();

    // 1. Try LightPanda native (fast, lightweight).
    if let Some((guard, ws_url)) = try_lightpanda_native().await {
        browsers.push((guard, ws_url, RendererKind::LightPanda));
    }

    // 2. Also try Chrome/Chromium native (robust for complex SPAs).
    if let Some((guard, ws_url)) = try_chrome_native().await {
        browsers.push((guard, ws_url, RendererKind::Chrome));
    }

    // 3. Docker only if nothing native was found (last resort).
    if browsers.is_empty()
        && let Some((guard, ws_url)) = try_lightpanda_docker().await
    {
        browsers.push((guard, ws_url, RendererKind::LightPanda));
    }

    browsers
}

// --- LightPanda native ---

/// Find LightPanda binary: PATH → ~/.crw/lightpanda → auto-download.
async fn find_or_download_lightpanda() -> Option<String> {
    // 1. Check PATH.
    if let Some(bin) = find_in_path("lightpanda") {
        tracing::info!("Found LightPanda in PATH: {bin}");
        return Some(bin);
    }

    // 2. Check ~/.crw/lightpanda (our managed install location).
    let managed_path = lightpanda_managed_path()?;
    if managed_path.exists() {
        let path_str = managed_path.to_string_lossy().to_string();
        tracing::info!("Found managed LightPanda: {path_str}");
        return Some(path_str);
    }

    // 3. Auto-download from GitHub releases.
    let download_url = lightpanda_download_url()?;
    tracing::info!("Downloading LightPanda from {download_url}...");

    if let Some(parent) = managed_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("Failed to create ~/.crw directory: {e}");
        return None;
    }

    let output = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(managed_path.as_os_str())
        .arg(&download_url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?
        .wait_with_output()
        .await
        .ok()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("Failed to download LightPanda: {stderr}");
        // Clean up partial download.
        let _ = std::fs::remove_file(&managed_path);
        return None;
    }

    // Make executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&managed_path, std::fs::Permissions::from_mode(0o755))
        {
            tracing::warn!("Failed to chmod LightPanda binary: {e}");
            let _ = std::fs::remove_file(&managed_path);
            return None;
        }
    }

    let path_str = managed_path.to_string_lossy().to_string();
    tracing::info!("LightPanda downloaded to {path_str}");
    Some(path_str)
}

/// Get the managed install path: ~/.crw/lightpanda
fn lightpanda_managed_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".crw").join("lightpanda"))
}

/// Get the correct download URL for the current platform.
fn lightpanda_download_url() -> Option<String> {
    let base = "https://github.com/lightpanda-io/browser/releases/download/nightly";

    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some(format!("{base}/lightpanda-aarch64-macos")),
        ("linux", "x86_64") => Some(format!("{base}/lightpanda-x86_64-linux")),
        ("linux", "aarch64") => Some(format!("{base}/lightpanda-aarch64-linux")),
        (os, arch) => {
            tracing::debug!("No LightPanda binary available for {os}/{arch}");
            None
        }
    }
}

/// Ranges [`crw_core::url_safety`] rejects that LightPanda's own
/// `--block-private-networks` group does not cover, so the browser we launch
/// enforces the same policy the rest of the pipeline does.
/// Omits `64:ff9b::/96`: [`crw_core::url_safety`] decodes that prefix and allows
/// a public embedded IPv4, because on an IPv6-only network with a DNS64/NAT64
/// resolver the whole v4 web resolves inside it, and a CIDR list cannot express
/// "carrying a private IPv4". `64:ff9b:1::/48` stays, RFC 8215 reserves it for
/// local use. `2002::/16` also stays even though `url_safety` decodes it too:
/// 6to4 is decommissioned (RFC 7526) so nothing real resolves there and the
/// stricter setting costs no recall, while this flag is the only control
/// covering worker targets, which never reach the CDP pump. ULA and v6
/// link-local are listed explicitly rather than assumed to be in LightPanda's
/// private group, and so are the IPv4 ranges that matter most (RFC1918,
/// loopback, link-local): this flag is the only control on the paths the CDP
/// pump cannot see, so it should not rest on an assumption about what upstream's
/// private group covers.
const LIGHTPANDA_EXTRA_BLOCK_CIDRS: &str = "0.0.0.0/8,10.0.0.0/8,127.0.0.0/8,169.254.0.0/16,172.16.0.0/12,192.168.0.0/16,100.64.0.0/10,224.0.0.0/4,240.0.0.0/4,\
192.0.0.0/24,192.0.2.0/24,198.18.0.0/15,198.51.100.0/24,203.0.113.0/24,\
fc00::/7,fe80::/10,fec0::/10,ff00::/8,::/96,64:ff9b:1::/48,2002::/16";

async fn try_lightpanda_native() -> Option<(ManagedBrowser, String)> {
    let bin = find_or_download_lightpanda().await?;

    // Find an available port for LightPanda.
    let port = find_available_port()?;
    let port_str = port.to_string();

    let mut cmd = Command::new(&bin);
    // Refuse private/internal destinations in the browser itself. This is the
    // only control that sees what the CDP interception pump cannot — websockets,
    // worker targets — and it runs on the resolved socket address, so it has no
    // time-of-check window. An older binary that does not know the flag exits,
    // the readiness poll below fails, and the ladder moves on.
    cmd.args([
        "serve",
        "--host",
        "127.0.0.1",
        "--port",
        &port_str,
        "--block-private-networks",
        "--block-cidrs",
        LIGHTPANDA_EXTRA_BLOCK_CIDRS,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    // Own process group: a group-kill reaps any LightPanda helper procs,
    // and detaching from crw's terminal group means Ctrl-C is delivered
    // by the teardown task, not twice. (Must ship with Phase 2 teardown.)
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd
        .spawn()
        .map_err(|e| tracing::warn!("Failed to spawn LightPanda: {e}"))
        .ok()?;

    // Register the pgid BEFORE readiness polling — there is a real leak
    // window if Ctrl-C lands during the 5s poll. Build the guard now so a
    // poll failure drops it (→ killpg + deregister) instead of orphaning.
    let pgid = register_child(&child);
    let guard = ManagedBrowser {
        kind: BrowserKind::Process { child, pgid },
    };

    // LightPanda doesn't print a WS URL to stderr like Chrome does.
    // Poll /json/version until it's ready (up to 5 seconds).
    let ws_url = poll_cdp_endpoint(port, 5).await?; // guard drops on None
    tracing::info!("LightPanda CDP endpoint: {ws_url}");

    Some((guard, ws_url))
}

// --- LightPanda Docker ---

async fn try_lightpanda_docker() -> Option<(ManagedBrowser, String)> {
    // Check if Docker is available.
    if !command_exists("docker") {
        return None;
    }

    tracing::info!("Trying LightPanda via Docker...");

    // `docker run --rm -d -p 0:9222` → random host port mapped to 9222.
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-d",
            "-p",
            "0:9222",
            "lightpanda/browser:latest",
            // Overrides the image CMD, so the whole serve line has to be
            // repeated. Same reasoning as the native launch above.
            "/bin/lightpanda",
            "serve",
            "--host",
            "0.0.0.0",
            "--port",
            "9222",
            "--block-private-networks",
            "--block-cidrs",
            LIGHTPANDA_EXTRA_BLOCK_CIDRS,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?
        .wait_with_output()
        .await
        .ok()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!("LightPanda Docker failed: {stderr}");
        return None;
    }

    let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if container_id.is_empty() {
        return None;
    }

    tracing::info!("LightPanda container started: {}", &container_id[..12]);

    // Get the mapped host port via `docker port`.
    let port = get_docker_mapped_port(&container_id, 9222).await?;

    // LightPanda needs a moment to start listening.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let ws_url = format!("ws://127.0.0.1:{port}/");
    tracing::info!("LightPanda Docker CDP endpoint: {ws_url}");

    Some((
        ManagedBrowser {
            kind: BrowserKind::Docker(container_id),
        },
        ws_url,
    ))
}

async fn get_docker_mapped_port(container_id: &str, container_port: u16) -> Option<u16> {
    let output = Command::new("docker")
        .args(["port", container_id, &container_port.to_string()])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    // Output format: "0.0.0.0:55000\n" or "0.0.0.0:55000\n:::55000\n"
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .next()?
        .rsplit(':')
        .next()?
        .trim()
        .parse()
        .ok()
}

// --- Chrome/Chromium native ---

const CHROME_CANDIDATES: &[&str] = &[
    // macOS
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    // Linux
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
];

fn find_chrome() -> Option<String> {
    for candidate in CHROME_CANDIDATES {
        let found = if candidate.starts_with('/') {
            std::path::Path::new(candidate).exists()
        } else {
            find_in_path(candidate).is_some()
        };
        if found {
            return Some(candidate.to_string());
        }
    }
    None
}

async fn try_chrome_native() -> Option<(ManagedBrowser, String)> {
    let bin = find_chrome()?;
    tracing::info!("Auto-detected Chrome: {bin}");

    let mut cmd = Command::new(&bin);
    cmd.args([
        "--headless",
        "--disable-gpu",
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--remote-debugging-port=0",
        "--remote-allow-origins=*",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    // Own process group so a group-kill reaps Chrome's zygote + renderer
    // children, not just the parent PID (rust-lang/rust#115241).
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| tracing::warn!("Failed to spawn Chrome: {e}"))
        .ok()?;

    // Take stderr before moving `child` into the guard; register the pgid
    // BEFORE reading the WS URL so a Ctrl-C during startup still reaps it.
    let stderr = child.stderr.take()?;
    let pgid = register_child(&child);
    let guard = ManagedBrowser {
        kind: BrowserKind::Process { child, pgid },
    };

    let ws_url = read_ws_url_from_stderr(stderr).await?; // guard drops on None
    tracing::info!("Chrome CDP endpoint: {ws_url}");
    Some((guard, ws_url))
}

// --- Shared helpers ---

/// Find an available TCP port by binding to port 0.
fn find_available_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

/// Poll a CDP endpoint's /json/version until it responds with a webSocketDebuggerUrl.
async fn poll_cdp_endpoint(port: u16, timeout_secs: u64) -> Option<String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    while tokio::time::Instant::now() < deadline {
        if let Ok(resp) = reqwest::get(&url).await
            && let Ok(json) = resp.json::<serde_json::Value>().await
            && let Some(ws_url) = json.get("webSocketDebuggerUrl").and_then(|v| v.as_str())
        {
            return Some(ws_url.to_string());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

/// Read the WebSocket URL from a browser's stderr pipe.
/// Chrome prints "DevTools listening on ws://...", LightPanda prints "Listening on ws://...".
async fn read_ws_url_from_stderr(stderr: tokio::process::ChildStderr) -> Option<String> {
    let mut reader = BufReader::new(stderr).lines();

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Ok(Some(line)) = reader.next_line().await {
            // Chrome: "DevTools listening on ws://127.0.0.1:PORT/devtools/browser/UUID"
            if let Some(url) = line.strip_prefix("DevTools listening on ") {
                return Some(url.trim().to_string());
            }
            // LightPanda or other: "Listening on ws://..."
            if let Some(start) = line.find("ws://") {
                return Some(line[start..].trim().to_string());
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

fn find_in_path(name: &str) -> Option<String> {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| name.to_string())
}

fn command_exists(name: &str) -> bool {
    find_in_path(name).is_some()
}
