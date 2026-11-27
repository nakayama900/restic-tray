//! `rclone serve s3` loopback adapter: runs rclone as a local S3 endpoint in front of any rclone
//! remote (including `crypt:` remotes for Rclone Crypto) so Restic keeps using its standard S3
//! backend against `http://127.0.0.1:<port>/<bucket>`.
//!
//! The adapter binds `127.0.0.1` only, allows anonymous access (no `--auth-key`), and is restarted
//! whenever the relevant rclone settings change. Restic receives non-secret placeholder S3
//! credentials because the loopback endpoint ignores authentication.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::{AppConfig, RcloneConfig, StorageBackend};
use crate::error::{AppError, AppResult};

/// Placeholder S3 credentials for the anonymous loopback adapter (not secrets).
pub const ADAPTER_ACCESS_KEY_ID: &str = "restic-tray-local";
pub const ADAPTER_SECRET_ACCESS_KEY: &str = "restic-tray-local";

/// How long to wait for the adapter port to accept TCP connections before failing.
const ADAPTER_START_TIMEOUT: Duration = Duration::from_secs(20);
const ADAPTER_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct RunningAdapter {
    child: Child,
    endpoint: String,
    settings_key: String,
}

static ADAPTER: OnceLock<Mutex<Option<RunningAdapter>>> = OnceLock::new();

fn adapter_cell() -> &'static Mutex<Option<RunningAdapter>> {
    ADAPTER.get_or_init(|| Mutex::new(None))
}

/// Resolves the rclone binary: an explicit override, then `rclone.exe` next to the app binary,
/// then the ambient `PATH`.
fn resolve_rclone_path(override_path: Option<&str>) -> AppResult<PathBuf> {
    let exe_name = if cfg!(windows) {
        "rclone.exe"
    } else {
        "rclone"
    };
    if let Some(custom) = override_path.map(str::trim).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Ok(p);
        }
        return Err(AppError::Config(format!(
            "rclone.binaryPath does not point to an existing file: {custom}"
        )));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let candidate = exe_dir.join(exe_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from(exe_name))
}

/// Picks the loopback port for the adapter: the configured one, or an ephemeral free port.
fn pick_port(configured: Option<u16>) -> io::Result<u16> {
    if let Some(port) = configured {
        return Ok(port);
    }
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

fn adapter_settings_key(rclone: &RcloneConfig) -> String {
    format!(
        "{}|{}|{}|{}",
        rclone.normalized_remote(),
        rclone.normalized_bucket(),
        rclone
            .binary_path
            .as_deref()
            .unwrap_or_default()
            .replace('/', "\\"),
        rclone
            .config_path
            .as_deref()
            .unwrap_or_default()
            .replace('/', "\\"),
    )
}

fn spawn_adapter(rclone: &RcloneConfig) -> AppResult<RunningAdapter> {
    let remote = rclone.validate_remote()?;
    let binary = resolve_rclone_path(rclone.binary_path.as_deref())?;
    let port = pick_port(rclone.port).map_err(|e| {
        AppError::Config(format!(
            "failed to allocate a loopback port for rclone serve s3: {e}"
        ))
    })?;
    let addr = format!("127.0.0.1:{port}");

    let mut cmd = Command::new(&binary);
    cmd.arg("serve")
        .arg("s3")
        .arg("--addr")
        .arg(&addr)
        .arg(&remote)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(config_path) = rclone
        .config_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let canonical = canonicalize_existing_file(config_path)?;
        cmd.env("RCLONE_CONFIG", &canonical);
    }
    apply_windows_creation_flags(&mut cmd);

    let child = cmd
        .spawn()
        .map_err(|e| AppError::Config(format!("failed to spawn rclone serve s3: {e}")))?;

    let endpoint = format!("http://{addr}");
    let settings_key = adapter_settings_key(rclone);
    Ok(RunningAdapter {
        child,
        endpoint,
        settings_key,
    })
}

fn apply_windows_creation_flags(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

fn canonicalize_existing_file(path: &str) -> AppResult<PathBuf> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(AppError::Config(format!(
            "rclone.configPath does not point to an existing file: {path}"
        )));
    }
    let canonical = dunce::canonicalize(p).map_err(|e| {
        AppError::Config(format!("failed to resolve rclone.configPath {}: {e}", path))
    })?;
    Ok(canonical)
}

/// Waits until the adapter accepts TCP connections on its port, or fails with the child's early
/// exit status when rclone terminates during startup.
fn wait_until_ready(child: &mut Child, addr: &str) -> AppResult<()> {
    let deadline = Instant::now() + ADAPTER_START_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(AppError::Config(format!(
                "rclone serve s3 exited during startup with status {status} (check rclone.remote and rclone.conf)"
            )));
        }
        if let Ok(addr) = addr.parse::<std::net::SocketAddr>() {
            let timeout = Duration::from_millis(200);
            if TcpStream::connect_timeout(&addr, timeout).is_ok() {
                return Ok(());
            }
        } else {
            return Err(AppError::Config(format!(
                "invalid rclone serve s3 address: {addr}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(AppError::Config(
                "timed out waiting for rclone serve s3 to listen on its loopback port".to_owned(),
            ));
        }
        std::thread::sleep(ADAPTER_POLL_INTERVAL);
    }
}

/// Ensures the `rclone serve s3` adapter is running for the current rclone settings and returns
/// the S3 endpoint URL (`http://127.0.0.1:<port>`). Restarts the adapter when the settings changed.
/// Only valid when `storageBackend` is `rclone`.
pub fn ensure_adapter(config: &AppConfig) -> AppResult<String> {
    if config.storage_backend != StorageBackend::Rclone {
        return Err(AppError::Config(
            "rclone serve s3 adapter requested while storageBackend is not \"rclone\"".to_owned(),
        ));
    }
    let rclone = &config.rclone;
    rclone.validate_remote()?;
    let settings_key = adapter_settings_key(rclone);
    let mut guard = adapter_cell()
        .lock()
        .map_err(|_| AppError::Config("rclone adapter lock poisoned".to_owned()))?;
    if let Some(running) = guard.as_mut() {
        if running.settings_key == settings_key
            && matches!(running.child.try_wait(), Ok(None))
            && port_open(&running.endpoint)
        {
            return Ok(running.endpoint.clone());
        }
        let _ = running.child.kill();
        let _ = running.child.wait();
        *guard = None;
    }
    let mut adapter = spawn_adapter(rclone)?;
    let addr = adapter.endpoint.trim_start_matches("http://").to_owned();
    wait_until_ready(&mut adapter.child, &addr)?;
    let endpoint = adapter.endpoint.clone();
    *guard = Some(adapter);
    Ok(endpoint)
}

fn port_open(endpoint: &str) -> bool {
    let Some(addr) = endpoint
        .trim_start_matches("http://")
        .parse::<std::net::SocketAddr>()
        .ok()
    else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// Stops the adapter, if running. Called on application exit and when switching backends.
pub fn shutdown() {
    let Ok(mut guard) = adapter_cell().lock() else {
        return;
    };
    if let Some(mut running) = guard.take() {
        let _ = running.child.kill();
        let _ = running.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_settings_key_reflects_relevant_settings() {
        let base = RcloneConfig {
            remote: "mycrypt:".to_owned(),
            ..Default::default()
        };
        let mut changed = base.clone();
        changed.remote = "mycrypt:sub".to_owned();
        assert_ne!(adapter_settings_key(&base), adapter_settings_key(&changed));
        let mut changed_port = base.clone();
        changed_port.port = Some(1234);
        assert_eq!(
            adapter_settings_key(&base),
            adapter_settings_key(&changed_port)
        );
    }

    #[test]
    fn resolve_rclone_path_rejects_missing_override() {
        assert!(resolve_rclone_path(Some("Z:/does/not/exist.exe")).is_err());
        assert!(resolve_rclone_path(None).is_ok());
    }

    #[test]
    fn canonicalize_existing_file_rejects_missing() {
        assert!(canonicalize_existing_file("Z:/missing/rclone.conf").is_err());
    }
}
