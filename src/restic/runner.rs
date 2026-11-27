//! Safe subprocess execution for `restic.exe` against Cloudflare R2.
//!
//! - Never invokes a shell (`cmd.exe` or `pwsh`).
//! - Injects R2 credentials and repository password via environment variables only.
//! - Suppresses console windows on Windows via `CREATE_NO_WINDOW`.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{AppConfig, BackupTarget};
use crate::error::{AppError, AppResult};
use crate::fs_safety::validate_restore_destination;
use crate::restic::excludes::{backup_filter_args, exclude_fingerprint};
use crate::restic::sidecar::resolve_restic_path;
use crate::restic::types::{
    BackupProgressState, FolderBytes, ResticBackupSummary, ResticDiffReport, ResticLsEntry,
    ResticSnapshot, TAG_EXCLUDES_PREFIX, TAG_HANDOFF_STAGING, TAG_PROJECT_PREFIX,
};
use crate::secrets::StoredSecrets;

/// Restic exit code returned when `--retry-lock` expires without acquiring a repository lock.
pub const RESTIC_EXIT_LOCK_TIMEOUT: i32 = 11;

pub const RETRY_LOCK_SCHEDULED_BACKUP: &str = "10m";
pub const RETRY_LOCK_MANUAL_OR_HANDOFF: &str = "2m";
pub const RETRY_LOCK_MAINTENANCE: &str = "5m";
pub const RETRY_LOCK_READ: &str = "30s";

pub const LOCK_WAIT_MESSAGE: &str = "他 PC がリポジトリを整理中のため待機しています";

/// Upper bound for buffered child stderr when streaming command output.
const MAX_CAPTURED_STDERR_BYTES: usize = 64 * 1024;

/// Validates a Restic snapshot identifier before passing it as a CLI argument.
///
/// Accepts `"latest"` (only when `allow_latest` is true) or 4–64 ASCII hexadecimal characters,
/// preventing leading-dash option injection (for example `--password-command=...`).
pub fn validate_snapshot_id(raw: &str, allow_latest: bool) -> AppResult<&str> {
    let trimmed = raw.trim();
    if allow_latest && trimmed == "latest" {
        return Ok("latest");
    }
    if (4..=64).contains(&trimmed.len()) && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(trimmed);
    }
    let expected = if allow_latest {
        "'latest' or 4–64 hexadecimal characters"
    } else {
        "4–64 hexadecimal characters"
    };
    Err(AppError::InvalidArgument(format!(
        "invalid snapshotId \"{trimmed}\": expected {expected}"
    )))
}

fn validate_no_control_chars<'a>(raw: &'a str, label: &str) -> AppResult<&'a str> {
    let trimmed = raw.trim();
    if trimmed.chars().any(char::is_control) {
        return Err(AppError::InvalidArgument(format!(
            "{label} must not contain control characters"
        )));
    }
    Ok(trimmed)
}

/// Reads child stderr up to `MAX_CAPTURED_STDERR_BYTES` while draining any remainder so the
/// child process never blocks on a full stderr pipe.
fn read_bounded_stderr(mut pipe: impl Read) -> Vec<u8> {
    let mut captured = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if captured.len() < MAX_CAPTURED_STDERR_BYTES {
                    let take = n.min(MAX_CAPTURED_STDERR_BYTES - captured.len());
                    captured.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    captured
}

/// Streams `restic ls --json` node lines from `reader`, returning at most `cap` entries and
/// whether reading stopped early upon reaching `cap`.
fn parse_ls_entries_streaming<R: BufRead>(mut reader: R, cap: usize) -> (Vec<ResticLsEntry>, bool) {
    let mut entries = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return (entries, false),
            Ok(_) => {
                let Ok(val) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if val.get("struct_type").and_then(Value::as_str) == Some("node")
                    && let Ok(entry) = serde_json::from_value::<ResticLsEntry>(val)
                {
                    entries.push(entry);
                    if entries.len() >= cap {
                        return (entries, true);
                    }
                }
            }
        }
    }
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

#[cfg(test)]
std::thread_local! {
    static TEST_LOCAL_REPO_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only RAII guard that routes `build_restic_command` on the current thread to a local
/// filesystem Restic repository so full end-to-end tests can exercise real `restic.exe` without
/// touching Cloudflare R2.
#[cfg(test)]
pub(crate) struct TestLocalRepoGuard {
    prev: Option<std::path::PathBuf>,
}

#[cfg(test)]
impl TestLocalRepoGuard {
    pub(crate) fn new(repo_path: impl Into<std::path::PathBuf>) -> Self {
        let prev =
            TEST_LOCAL_REPO_OVERRIDE.with(|cell| cell.borrow_mut().replace(repo_path.into()));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for TestLocalRepoGuard {
    fn drop(&mut self) {
        TEST_LOCAL_REPO_OVERRIDE.with(|cell| {
            *cell.borrow_mut() = self.prev.take();
        });
    }
}

/// Build a configured `std::process::Command` for `restic` with Cloudflare R2 environment variables.
fn build_restic_command(config: &AppConfig, secrets: &StoredSecrets) -> AppResult<Command> {
    #[cfg(test)]
    if let Some(local_repo) = TEST_LOCAL_REPO_OVERRIDE.with(|cell| cell.borrow().clone()) {
        let password = secrets
            .restic_password
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::Config("Restic repository password is not set".to_owned()))?;
        let (bin_path, _) = resolve_restic_path(config.restic_binary_path.as_deref());
        let mut cmd = Command::new(bin_path);
        cmd.env("RESTIC_REPOSITORY", local_repo)
            .env("RESTIC_PASSWORD", password);
        apply_windows_creation_flags(&mut cmd);
        return Ok(cmd);
    }

    let repo_url = config.repository_url()?;
    let password = secrets
        .restic_password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Config("Restic repository password is not set".to_owned()))?;
    let access_key = secrets
        .r2_access_key_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Config("R2 Access Key ID is not set in DPAPI store".to_owned()))?;
    let secret_key = secrets
        .r2_secret_access_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Config("R2 Secret Access Key is not set in DPAPI store".to_owned())
        })?;

    let (bin_path, _) = resolve_restic_path(config.restic_binary_path.as_deref());
    let mut cmd = Command::new(bin_path);
    cmd.env("RESTIC_REPOSITORY", &repo_url)
        .env("RESTIC_PASSWORD", password)
        .env("AWS_ACCESS_KEY_ID", access_key)
        .env("AWS_SECRET_ACCESS_KEY", secret_key)
        .env("AWS_DEFAULT_REGION", config.r2.region.trim())
        .arg("-o")
        .arg(format!(
            "s3.connections={}",
            config.r2.max_connections.max(1)
        ));
    apply_windows_creation_flags(&mut cmd);

    Ok(cmd)
}

fn classify_restic_error(status: ExitStatus, name: &str, stderr: &str) -> AppError {
    let trimmed = stderr.trim();
    if status.code() == Some(RESTIC_EXIT_LOCK_TIMEOUT) {
        AppError::ResticLockTimeout(format!(
            "restic {name} timed out waiting for repository lock: {trimmed}"
        ))
    } else {
        AppError::Restic(format!("restic {name} failed: {trimmed}"))
    }
}

fn apply_retry_lock(cmd: &mut Command, retry_lock: Option<&str>) {
    if let Some(dur) = retry_lock.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.arg("--retry-lock").arg(dur);
    }
}

fn is_lock_wait_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("retrying lock")
        || lower.contains("unable to create lock")
        || lower.contains("repository is already locked")
}

/// Run `restic <name>` to the end and return its trimmed standard output, or its standard
/// error as the failure (returning `AppError::ResticLockTimeout` on exit code 11).
fn run_to_end(mut cmd: Command, name: &str) -> AppResult<String> {
    let output = cmd
        .output()
        .map_err(|e| AppError::Restic(format!("failed to spawn restic {name}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(classify_restic_error(output.status, name, &stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Same as `run_to_end`, but polls `cancel_flag` every 100 ms and kills the child immediately
/// when cancelled (even while waiting inside `--retry-lock`).
fn run_to_end_cancellable(
    mut cmd: Command,
    name: &str,
    cancel_flag: &Arc<AtomicBool>,
) -> AppResult<String> {
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(AppError::Restic(format!(
            "restic {name} cancelled by user or agent"
        )));
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::Restic(format!("failed to spawn restic {name}: {e}")))?;
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Restic(format!("missing restic {name} stdout")))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Restic(format!("missing restic {name} stderr")))?;

    let stdout_reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut out);
        out
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut out);
        out
    });

    let status = loop {
        // Check `try_wait` before `cancel_flag` so a subprocess (e.g. `restic tag`) that already
        // finished committing and exited cleanly is never falsely reported as cancelled.
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if cancel_flag.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(AppError::Restic(format!(
                        "restic {name} cancelled by user or agent"
                    )));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(AppError::Restic(format!(
                    "failed waiting on restic {name}: {e}"
                )));
            }
        }
    };

    let stdout_bytes = stdout_reader.join().unwrap_or_default();
    let stderr_bytes = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        return Err(classify_restic_error(status, name, &stderr));
    }
    Ok(String::from_utf8_lossy(&stdout_bytes).trim().to_owned())
}

/// Initialize the repository (`restic init --json`).
pub fn init_repository(config: &AppConfig, secrets: &StoredSecrets) -> AppResult<String> {
    let mut cmd = build_restic_command(config, secrets)?;
    cmd.arg("init").arg("--json");
    run_to_end(cmd, "init")
}

/// Check repository integrity (`restic check`).
pub fn check_repository(config: &AppConfig, secrets: &StoredSecrets) -> AppResult<String> {
    check_repository_with_retry(config, secrets, Some(RETRY_LOCK_MANUAL_OR_HANDOFF))
}

pub fn check_repository_with_retry(
    config: &AppConfig,
    secrets: &StoredSecrets,
    retry_lock: Option<&str>,
) -> AppResult<String> {
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, retry_lock);
    cmd.arg("check");
    run_to_end(cmd, "check")
}

/// Check the repository and download `percent` of its data to verify it can be restored
/// (`restic check --read-data-subset`). Takes the repository's exclusive lock.
pub fn verify_repository(
    config: &AppConfig,
    secrets: &StoredSecrets,
    percent: u32,
) -> AppResult<String> {
    verify_repository_with_retry(config, secrets, percent, Some(RETRY_LOCK_MAINTENANCE))
}

pub fn verify_repository_with_retry(
    config: &AppConfig,
    secrets: &StoredSecrets,
    percent: u32,
    retry_lock: Option<&str>,
) -> AppResult<String> {
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, retry_lock);
    cmd.arg("check")
        .arg("--read-data-subset")
        .arg(format!("{}%", percent.clamp(1, 100)));
    run_to_end(cmd, "check")
}

/// Remove stale locks safely (`restic unlock`, never `--remove-all`).
pub fn unlock_repository(config: &AppConfig, secrets: &StoredSecrets) -> AppResult<String> {
    let cmd = build_unlock_command(config, secrets)?;
    run_to_end(cmd, "unlock")
}

fn build_unlock_command(config: &AppConfig, secrets: &StoredSecrets) -> AppResult<Command> {
    let mut cmd = build_restic_command(config, secrets)?;
    cmd.arg("unlock");
    Ok(cmd)
}

/// List snapshots in the repository (`restic snapshots --json`).
pub fn list_snapshots(
    config: &AppConfig,
    secrets: &StoredSecrets,
    tag_filter: Option<&str>,
    path_filter: Option<&str>,
) -> AppResult<Vec<ResticSnapshot>> {
    list_snapshots_filtered(
        config,
        secrets,
        None,
        tag_filter,
        path_filter,
        Some(RETRY_LOCK_READ),
    )
}

/// List snapshots with optional host, tag, path filters and `--retry-lock`.
pub fn list_snapshots_filtered(
    config: &AppConfig,
    secrets: &StoredSecrets,
    host_filter: Option<&str>,
    tag_filter: Option<&str>,
    path_filter: Option<&str>,
    retry_lock: Option<&str>,
) -> AppResult<Vec<ResticSnapshot>> {
    list_snapshots_filtered_inner(
        config,
        secrets,
        host_filter,
        tag_filter,
        path_filter,
        retry_lock,
        None,
    )
}

/// Cancellable variant of `list_snapshots_filtered` used during handoff workflows.
pub fn list_snapshots_filtered_cancellable(
    config: &AppConfig,
    secrets: &StoredSecrets,
    host_filter: Option<&str>,
    tag_filter: Option<&str>,
    path_filter: Option<&str>,
    retry_lock: Option<&str>,
    cancel_flag: &Arc<AtomicBool>,
) -> AppResult<Vec<ResticSnapshot>> {
    list_snapshots_filtered_inner(
        config,
        secrets,
        host_filter,
        tag_filter,
        path_filter,
        retry_lock,
        Some(cancel_flag),
    )
}

fn list_snapshots_filtered_inner(
    config: &AppConfig,
    secrets: &StoredSecrets,
    host_filter: Option<&str>,
    tag_filter: Option<&str>,
    path_filter: Option<&str>,
    retry_lock: Option<&str>,
    cancel_flag: Option<&Arc<AtomicBool>>,
) -> AppResult<Vec<ResticSnapshot>> {
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, retry_lock);
    cmd.arg("snapshots").arg("--json");
    if let Some(host) = host_filter.map(str::trim).filter(|s| !s.is_empty()) {
        let host = validate_no_control_chars(host, "host")?;
        cmd.arg(format!("--host={host}"));
    }
    if let Some(tag) = tag_filter.map(str::trim).filter(|s| !s.is_empty()) {
        let tag = validate_no_control_chars(tag, "tag")?;
        cmd.arg(format!("--tag={tag}"));
    }
    if let Some(path) = path_filter.map(str::trim).filter(|s| !s.is_empty()) {
        let path = validate_no_control_chars(path, "path")?;
        cmd.arg(format!("--path={path}"));
    }

    let stdout = match cancel_flag {
        Some(flag) => run_to_end_cancellable(cmd, "snapshots", flag)?,
        None => run_to_end(cmd, "snapshots")?,
    };
    if stdout.is_empty() || stdout == "null" {
        return Ok(Vec::new());
    }
    serde_json::from_str(&stdout)
        .map_err(|e| AppError::Restic(format!("invalid snapshots JSON: {e}")))
}

fn build_list_snapshot_files_command(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_id: &str,
    prefix: Option<&str>,
) -> AppResult<Command> {
    let snap = validate_snapshot_id(snapshot_id, true)?;
    let clean_prefix = match prefix.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => Some(validate_no_control_chars(p, "prefix")?),
        None => None,
    };

    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(RETRY_LOCK_READ));
    cmd.arg("ls").arg("--json").arg("--").arg(snap);
    if let Some(p) = clean_prefix {
        cmd.arg(p);
    }
    Ok(cmd)
}

/// List files inside a snapshot (`restic ls --json -- <snapshot_id> [prefix]`), streaming rows
/// so memory stays bounded to `max_entries` (capped at 2,000).
pub fn list_snapshot_files(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_id: &str,
    prefix: Option<&str>,
    max_entries: usize,
) -> AppResult<Vec<ResticLsEntry>> {
    let mut cmd = build_list_snapshot_files_command(config, secrets, snapshot_id, prefix)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::Restic(format!("failed to spawn restic ls: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Restic("missing child stdout".to_owned()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Restic("missing child stderr".to_owned()))?;
    let stderr_reader = std::thread::spawn(move || read_bounded_stderr(stderr_pipe));

    let cap = max_entries.clamp(1, 2000);
    let (entries, stopped_early) = {
        let reader = BufReader::new(stdout);
        parse_ls_entries_streaming(reader, cap)
    };

    if stopped_early {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_reader.join();
        return Ok(entries);
    }

    let status = child
        .wait()
        .map_err(|e| AppError::Restic(format!("failed waiting on restic ls: {e}")))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(classify_restic_error(
            status,
            "ls",
            &String::from_utf8_lossy(&stderr),
        ));
    }
    Ok(entries)
}

fn build_snapshot_folder_sizes_command(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_id: &str,
) -> AppResult<Command> {
    let snap = validate_snapshot_id(snapshot_id, true)?;
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(RETRY_LOCK_READ));
    cmd.arg("ls").arg("--json").arg("--").arg(snap);
    Ok(cmd)
}

/// File sizes in a snapshot summed per folder, largest first, with folders grouped as in
/// `top_added`. Reads `restic ls --json` line by line: the listing has one line per file and
/// is never held in memory.
pub fn snapshot_folder_sizes(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_id: &str,
    targets: &[BackupTarget],
) -> AppResult<Vec<FolderBytes>> {
    let mut cmd = build_snapshot_folder_sizes_command(config, secrets, snapshot_id)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::Restic(format!("failed to spawn restic ls: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Restic("missing child stdout".to_owned()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Restic("missing child stderr".to_owned()))?;
    let stderr_reader = std::thread::spawn(move || read_bounded_stderr(stderr_pipe));

    let mut sizes = AddedTally {
        targets,
        bytes: HashMap::new(),
    };
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Some((path, bytes)) = ls_file_size(&line) {
            sizes.add(&path, bytes);
        }
    }

    let status = child
        .wait()
        .map_err(|e| AppError::Restic(format!("failed waiting on restic: {e}")))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(classify_restic_error(
            status,
            "ls",
            &String::from_utf8_lossy(&stderr),
        ));
    }
    Ok(sizes.top(usize::MAX))
}

/// Path and size of a file line from `restic ls --json`; `None` for folders and other lines.
fn ls_file_size(line: &str) -> Option<(String, u64)> {
    // Skip folder lines before parsing any JSON.
    if !line.contains(r#""type":"file""#) {
        return None;
    }
    let val: Value = serde_json::from_str(line).ok()?;
    let path = val.get("path")?.as_str()?.to_owned();
    Some((path, val.get("size").and_then(Value::as_u64).unwrap_or(0)))
}

fn build_diff_snapshots_command(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_a: &str,
    snapshot_b: &str,
) -> AppResult<(Command, String, String)> {
    let a = validate_snapshot_id(snapshot_a, true)?;
    let b = validate_snapshot_id(snapshot_b, true)?;
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(RETRY_LOCK_READ));
    cmd.arg("diff").arg("--json").arg("--").arg(a).arg(b);
    Ok((cmd, a.to_owned(), b.to_owned()))
}

/// Compare two snapshots (`restic diff --json -- <s1> <s2>`).
pub fn diff_snapshots(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_a: &str,
    snapshot_b: &str,
) -> AppResult<ResticDiffReport> {
    let (cmd, a, b) = build_diff_snapshots_command(config, secrets, snapshot_a, snapshot_b)?;
    let stdout = run_to_end(cmd, "diff")?;

    let mut report = ResticDiffReport {
        source_snapshot: a,
        target_snapshot: b,
        ..Default::default()
    };

    for line in stdout.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match val.get("message_type").and_then(Value::as_str) {
            Some("change") => {
                if report.changed_paths.len() < 500
                    && let Some(p) = val.get("path").and_then(Value::as_str)
                {
                    let mod_kind = val.get("modifier").and_then(Value::as_str).unwrap_or("?");
                    report.changed_paths.push(format!("{mod_kind} {p}"));
                }
            }
            Some("statistics") => {
                if let Some(added) = val.get("added") {
                    report.added_files = added.get("files").and_then(Value::as_u64).unwrap_or(0);
                    report.added_bytes = added.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                }
                if let Some(removed) = val.get("removed") {
                    report.removed_files =
                        removed.get("files").and_then(Value::as_u64).unwrap_or(0);
                    report.removed_bytes =
                        removed.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                }
                report.modified_files = val
                    .get("changed_files")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
            _ => {}
        }
    }

    Ok(report)
}

/// Options controlling a single-target `restic backup` execution (1 target = 1 snapshot).
pub struct BackupTargetOptions<'a> {
    pub dry_run: bool,
    /// Free-form user/MCP tags (validated to forbid reserved `project:` / `handoff:` / `excludes:`
    /// namespaces and commas).
    pub extra_tags: &'a [String],
    /// Internal coordination tags emitted only by Restic Tray (e.g. `handoff:released`,
    /// `handoff:claimed`, `handoff:safety-backup`, `handoff:staging`).
    pub internal_tags: &'a [String],
    /// When true, omits the `project:<key>` tag during backup so a staging handoff snapshot is
    /// invisible to other PCs until it finishes with exit code 0 and is atomically promoted via
    /// `promote_handoff_snapshot`.
    pub suppress_project_tag: bool,
    pub retry_lock: &'a str,
    pub parent_snapshot: Option<&'a str>,
    pub progress_label: Option<&'a str>,
}

fn build_backup_target_command(
    config: &AppConfig,
    secrets: &StoredSecrets,
    target: &BackupTarget,
    opts: &BackupTargetOptions<'_>,
) -> AppResult<(Command, String)> {
    let single = std::slice::from_ref(target);
    let filter_args = backup_filter_args(config, single)?;
    let fingerprint = exclude_fingerprint(config, single);
    let validated_target_tags = crate::config::validate_user_tags(&target.tags)?;
    let validated_extra_tags = crate::config::validate_user_tags(opts.extra_tags)?;

    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(opts.retry_lock));
    // `-vv` adds a `verbose_status` line per file with the repository bytes it stored.
    cmd.arg("backup")
        .arg("--json")
        .arg("-vv")
        .arg("--host")
        .arg(config.effective_machine_name());
    if opts.dry_run {
        cmd.arg("--dry-run");
    }
    if let Some(parent) = opts
        .parent_snapshot
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let valid_parent = validate_snapshot_id(parent, false)?;
        cmd.arg("--parent").arg(valid_parent);
    }
    cmd.args(filter_args);

    let mut tags: Vec<String> = Vec::new();
    let mut push_tag = |tag: String| {
        let trimmed = tag.trim();
        if !trimmed.is_empty() && !tags.iter().any(|existing| existing == trimmed) {
            tags.push(trimmed.to_owned());
        }
    };
    if let Some(key) = target.project_key.as_deref() {
        if !opts.suppress_project_tag {
            push_tag(format!("{TAG_PROJECT_PREFIX}{key}"));
        }
        push_tag(format!("{TAG_EXCLUDES_PREFIX}{fingerprint}"));
    }
    for tag in opts.internal_tags {
        push_tag(tag.clone());
    }
    for tag in validated_target_tags {
        push_tag(tag);
    }
    for tag in validated_extra_tags {
        push_tag(tag);
    }
    for tag in tags {
        cmd.arg("--tag").arg(tag);
    }
    cmd.arg("--").arg(&target.path);

    Ok((cmd, fingerprint))
}

/// Three-way outcome of promoting a `handoff:staging` snapshot via `restic tag`:
/// - `Confirmed`: `restic tag` succeeded AND post-tag listing verified `original == staging_id`.
/// - `NotFoundAfterLookup`: `restic tag` exited 0 (e.g. Restic 0.17.3 `cmd_tag.go` per-snapshot
///   warning path), and a successful listing confirmed no promoted snapshot exists.
/// - `LookupUncertain`: `restic tag` exited 0, but the post-tag lookup failed/cancelled so
///   publication state must be resolved on retry rather than rolling back `Standby` or clearing
///   `"releasing"`.
pub enum PromoteHandoffOutcome {
    Confirmed {
        snapshot_id: String,
        project_snapshots: Vec<ResticSnapshot>,
    },
    NotFoundAfterLookup,
    LookupUncertain(String),
}

/// Atomically promotes a clean (exit code 0) `handoff:staging` snapshot into a published
/// `project:<project_key>` + `handoff_tag` (+ any `extra_add_tags`, e.g. `handoff:from:<src>`)
/// snapshot via `restic tag`.
#[allow(clippy::too_many_arguments)]
pub fn promote_handoff_snapshot(
    config: &AppConfig,
    secrets: &StoredSecrets,
    staging_snapshot_id: &str,
    project_key: &str,
    handoff_tag: &str,
    extra_add_tags: &[String],
    retry_lock: &str,
    cancel_flag: &Arc<AtomicBool>,
) -> AppResult<PromoteHandoffOutcome> {
    let staging_id = validate_snapshot_id(staging_snapshot_id, false)?;
    let project_tag = format!("{TAG_PROJECT_PREFIX}{project_key}");
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(retry_lock));
    cmd.arg("tag")
        .arg("--add")
        .arg(&project_tag)
        .arg("--add")
        .arg(handoff_tag);
    for extra in extra_add_tags {
        if !extra.trim().is_empty() {
            cmd.arg("--add").arg(extra.trim());
        }
    }
    cmd.arg("--remove")
        .arg(TAG_HANDOFF_STAGING)
        .arg("--")
        .arg(staging_id);
    run_to_end_cancellable(cmd, "tag", cancel_flag)?;

    match list_snapshots_filtered_cancellable(
        config,
        secrets,
        None,
        Some(&project_tag),
        None,
        Some(retry_lock),
        cancel_flag,
    ) {
        Ok(snaps) => {
            if let Some(promoted_id) = snaps
                .iter()
                .find(|s| s.original.as_deref() == Some(staging_id))
                .map(|s| s.id.clone())
            {
                Ok(PromoteHandoffOutcome::Confirmed {
                    snapshot_id: promoted_id,
                    project_snapshots: snaps,
                })
            } else {
                Ok(PromoteHandoffOutcome::NotFoundAfterLookup)
            }
        }
        Err(err) => Ok(PromoteHandoffOutcome::LookupUncertain(err.to_string())),
    }
}

/// Run `restic backup --json` for a single target (1 target = 1 snapshot), streaming progress
/// into `progress_state` and supporting immediate cancellation even while waiting on `--retry-lock`.
pub fn run_backup_target(
    config: &AppConfig,
    secrets: &StoredSecrets,
    target: &BackupTarget,
    opts: &BackupTargetOptions<'_>,
    progress_state: &Arc<Mutex<BackupProgressState>>,
    cancel_flag: &Arc<AtomicBool>,
) -> AppResult<(ResticBackupSummary, u64)> {
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(AppError::Restic(
            "backup cancelled by user or agent".to_owned(),
        ));
    }
    let (mut cmd, fingerprint) = build_backup_target_command(config, secrets, target, opts)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let started = Instant::now();
    let label = opts
        .progress_label
        .map(str::to_owned)
        .unwrap_or_else(|| target.label.clone());

    if let Ok(mut guard) = progress_state.lock() {
        *guard = BackupProgressState {
            running: true,
            target_label: label.clone(),
            ..Default::default()
        };
    }

    let mut child = cmd.spawn().map_err(|e| {
        if let Ok(mut guard) = progress_state.lock() {
            guard.running = false;
        }
        AppError::Restic(format!("failed to spawn restic backup: {e}"))
    })?;

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Restic("missing child stdout".to_owned()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Restic("missing child stderr".to_owned()))?;

    let stderr_progress = Arc::clone(progress_state);
    let stderr_reader = std::thread::spawn(move || {
        let mut collected = String::new();
        for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
            if is_lock_wait_line(&line)
                && let Ok(mut guard) = stderr_progress.lock()
            {
                guard.waiting_for_lock = true;
                guard.status_note = Some(LOCK_WAIT_MESSAGE.to_owned());
            }
            if !collected.is_empty() {
                collected.push('\n');
            }
            collected.push_str(&line);
        }
        collected
    });

    let stdout_progress = Arc::clone(progress_state);
    let stdout_label = label.clone();
    let target_clone = target.clone();
    let stdout_reader = std::thread::spawn(move || {
        let mut summary = ResticBackupSummary::default();
        let single = [target_clone];
        let mut added = AddedTally {
            targets: &single,
            bytes: HashMap::new(),
        };
        for line in BufReader::new(stdout_pipe).lines().map_while(Result::ok) {
            // `-vv` also reports every unchanged file; skip those before parsing any JSON.
            if line.contains(r#""message_type":"verbose_status""#) {
                if !line.contains(r#""action":"unchanged""#) {
                    added.add_line(&line);
                }
                continue;
            }
            parse_backup_json_line(&line, &stdout_label, &stdout_progress, &mut summary);
        }
        summary.top_added = added.top(5);
        summary
    });

    let status = loop {
        if cancel_flag.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            if let Ok(mut guard) = progress_state.lock() {
                guard.running = false;
                guard.waiting_for_lock = false;
                guard.status_note = None;
            }
            return Err(AppError::Restic(
                "backup cancelled by user or agent".to_owned(),
            ));
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                if let Ok(mut guard) = progress_state.lock() {
                    guard.running = false;
                    guard.waiting_for_lock = false;
                    guard.status_note = None;
                }
                return Err(AppError::Restic(format!("failed waiting on restic: {e}")));
            }
        }
    };

    let mut summary = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    summary.exclude_fingerprint = Some(fingerprint);

    if let Ok(mut guard) = progress_state.lock() {
        guard.running = false;
        guard.waiting_for_lock = false;
        guard.status_note = None;
    }

    // Exit code 3: restic saved the snapshot but could not read some source files.
    if status.code() == Some(3) && summary.snapshot_id.is_some() {
        summary.partial_read_warning = true;
        summary.unreadable = unreadable_items(&stderr);
    } else if !status.success() {
        return Err(classify_restic_error(status, "backup", &stderr));
    }

    // restic names the snapshot a dry run would have saved; the repository never holds it.
    if opts.dry_run {
        summary.snapshot_id = None;
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    Ok((summary, elapsed_ms))
}

/// Returns true when a Restic `verbose_status` item path (e.g. `/D/Projects/ProjectAlpha/Assets/`)
/// is the target directory itself or a descendant inside it, ignoring ancestor directories whose
/// mtime changes whenever sibling folders outside the target are modified.
fn is_item_within_target_subpath(item: &str, target_subpath: &str) -> bool {
    let item_clean = item.trim_end_matches('/');
    let root_clean = target_subpath.trim_end_matches('/');
    if item_clean.is_empty() || root_clean.is_empty() {
        return false;
    }
    if item_clean.eq_ignore_ascii_case(root_clean) {
        return true;
    }
    item_clean.len() > root_clean.len()
        && item_clean
            .get(..root_clean.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(root_clean))
        && item_clean.as_bytes().get(root_clean.len()) == Some(&b'/')
}

/// Checks whether a local target folder has additions, modifications, or deletions compared to
/// `parent_snapshot` (or Restic's latest parent for this host/path) using `restic backup --dry-run`.
/// Ignores ancestor-directory metadata changes above `target.path` (e.g. edits in sibling folders).
pub fn check_target_local_changes(
    config: &AppConfig,
    secrets: &StoredSecrets,
    target: &BackupTarget,
    parent_snapshot: Option<&str>,
) -> AppResult<bool> {
    check_target_local_changes_inner(config, secrets, target, parent_snapshot, None)
}

/// Cancellable variant of `check_target_local_changes` used during handoff workflows.
pub fn check_target_local_changes_cancellable(
    config: &AppConfig,
    secrets: &StoredSecrets,
    target: &BackupTarget,
    parent_snapshot: Option<&str>,
    cancel_flag: &Arc<AtomicBool>,
) -> AppResult<bool> {
    check_target_local_changes_inner(config, secrets, target, parent_snapshot, Some(cancel_flag))
}

fn check_target_local_changes_inner(
    config: &AppConfig,
    secrets: &StoredSecrets,
    target: &BackupTarget,
    parent_snapshot: Option<&str>,
    cancel_flag: Option<&Arc<AtomicBool>>,
) -> AppResult<bool> {
    let opts = BackupTargetOptions {
        dry_run: true,
        extra_tags: &[],
        internal_tags: &[],
        suppress_project_tag: false,
        retry_lock: RETRY_LOCK_READ,
        parent_snapshot,
        progress_label: None,
    };
    let (cmd, _) = build_backup_target_command(config, secrets, target, &opts)?;
    let stdout = match cancel_flag {
        Some(flag) => run_to_end_cancellable(cmd, "backup --dry-run", flag)?,
        None => run_to_end(cmd, "backup --dry-run")?,
    };

    let target_subpath = crate::fs_safety::windows_path_to_restic_subpath(&target.path).ok();
    let mut summary = ResticBackupSummary::default();
    let mut target_subtree_changed = false;

    for line in stdout.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match val.get("message_type").and_then(Value::as_str) {
            Some("verbose_status") => {
                let action = val.get("action").and_then(Value::as_str);
                if matches!(action, Some("new" | "modified"))
                    && let Some(item) = val.get("item").and_then(Value::as_str)
                    && let Some(ref root_sub) = target_subpath
                    && is_item_within_target_subpath(item, root_sub)
                {
                    target_subtree_changed = true;
                }
            }
            Some("summary") => {
                if let Ok(parsed) = serde_json::from_value::<ResticBackupSummary>(val) {
                    summary = parsed;
                }
            }
            _ => {}
        }
    }

    if summary.files_new > 0 || summary.files_changed > 0 || summary.dirs_new > 0 {
        return Ok(true);
    }
    if target_subpath.is_some() {
        Ok(target_subtree_changed)
    } else {
        Ok(summary.dirs_changed > 0)
    }
}

/// Repository bytes of new and modified files from restic's `verbose_status` lines, summed per
/// folder at most two levels below the backup target that holds the file.
struct AddedTally<'a> {
    targets: &'a [BackupTarget],
    bytes: HashMap<String, u64>,
}

impl AddedTally<'_> {
    fn add_line(&mut self, line: &str) {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let action = val.get("action").and_then(Value::as_str);
        let Some(item) = val.get("item").and_then(Value::as_str) else {
            return;
        };
        let bytes = val
            .get("data_size_in_repo")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // Folder items end with `/` and carry only metadata.
        if !matches!(action, Some("new" | "modified")) || item.ends_with('/') || bytes == 0 {
            return;
        }
        self.add(item, bytes);
    }

    fn add(&mut self, item: &str, bytes: u64) {
        if let Some(folder) = added_folder(self.targets, item) {
            *self.bytes.entry(folder).or_default() += bytes;
        }
    }

    fn top(self, n: usize) -> Vec<FolderBytes> {
        let mut folders: Vec<FolderBytes> = self
            .bytes
            .into_iter()
            .map(|(path, bytes)| FolderBytes { path, bytes })
            .collect();
        folders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
        folders.truncate(n);
        folders
    }
}

/// Windows folder for restic's `/D/dir/file` item: the backup target holding it plus at most
/// two folders below, e.g. `D:\Projects\ProjectAlpha\_Backups\20261109_x`.
fn added_folder(targets: &[BackupTarget], item: &str) -> Option<String> {
    let mut parts: Vec<String> = item
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect();
    parts.pop()?;
    parts.first_mut()?.push(':');
    targets.iter().find_map(|target| {
        let root: Vec<&str> = target.path.split('\\').filter(|c| !c.is_empty()).collect();
        let inside = root.len() <= parts.len()
            && root
                .iter()
                .zip(&parts)
                .all(|(a, b)| a.to_lowercase() == b.to_lowercase());
        inside.then(|| parts[..(root.len() + 2).min(parts.len())].join("\\"))
    })
}

/// Paths named in restic's JSON error lines on stderr, each once.
fn unreadable_items(stderr: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for line in stderr.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("message_type").and_then(Value::as_str) == Some("error")
            && let Some(item) = val.get("item").and_then(Value::as_str)
            && !items.iter().any(|known| known == item)
        {
            items.push(item.to_owned());
        }
    }
    if items.is_empty() {
        items.push(stderr.trim().lines().last().unwrap_or_default().to_owned());
    }
    items
}

pub fn parse_backup_json_line(
    line: &str,
    label: &str,
    progress_state: &Arc<Mutex<BackupProgressState>>,
    summary: &mut ResticBackupSummary,
) {
    let Ok(val) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match val.get("message_type").and_then(Value::as_str) {
        Some("status") => {
            if let Ok(mut guard) = progress_state.lock() {
                guard.running = true;
                guard.waiting_for_lock = false;
                guard.status_note = None;
                guard.target_label = label.to_owned();
                guard.percent_done = val
                    .get("percent_done")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                guard.files_done = val.get("files_done").and_then(Value::as_u64).unwrap_or(0);
                guard.total_files = val.get("total_files").and_then(Value::as_u64).unwrap_or(0);
                guard.bytes_done = val.get("bytes_done").and_then(Value::as_u64).unwrap_or(0);
                guard.total_bytes = val.get("total_bytes").and_then(Value::as_u64).unwrap_or(0);
                if let Some(curr) = val
                    .get("current_files")
                    .and_then(Value::as_array)
                    .and_then(|arr| arr.first())
                    .and_then(Value::as_str)
                {
                    guard.current_file = curr.to_owned();
                }
            }
        }
        Some("summary") => {
            if let Ok(parsed) = serde_json::from_value::<ResticBackupSummary>(val) {
                *summary = parsed;
            }
        }
        _ => {}
    }
}

/// Parses a single JSON line from `restic restore --json` into `BackupProgressState`.
pub fn parse_restore_json_line(
    line: &str,
    label: &str,
    progress_state: &Arc<Mutex<BackupProgressState>>,
) {
    let Ok(val) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if val.get("message_type").and_then(Value::as_str) == Some("status")
        && let Ok(mut guard) = progress_state.lock()
    {
        guard.running = true;
        guard.waiting_for_lock = false;
        guard.status_note = Some("引き継ぎデータを復元しています".to_owned());
        guard.target_label = label.to_owned();
        guard.percent_done = val
            .get("percent_done")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        guard.files_done = val
            .get("files_restored")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        guard.total_files = val.get("total_files").and_then(Value::as_u64).unwrap_or(0);
        guard.bytes_done = val
            .get("bytes_restored")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        guard.total_bytes = val.get("total_bytes").and_then(Value::as_u64).unwrap_or(0);
    }
}

fn build_restore_snapshot_command(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snap: &str,
    safe_target: &Path,
    validated_includes: &[&str],
) -> AppResult<Command> {
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(RETRY_LOCK_MANUAL_OR_HANDOFF));
    cmd.arg("restore").arg("--target").arg(safe_target);
    for inc in validated_includes {
        cmd.arg(format!("--include={inc}"));
    }
    cmd.arg("--").arg(snap);
    Ok(cmd)
}

/// Restore a snapshot into a validated safe destination folder (`restic restore --target <dir> -- <id>`).
pub fn restore_snapshot(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_id: &str,
    target_dir: &Path,
    include_paths: &[String],
) -> AppResult<String> {
    let snap = validate_snapshot_id(snapshot_id, true)?;
    let mut validated_includes = Vec::with_capacity(include_paths.len());
    for inc in include_paths {
        let clean = validate_no_control_chars(inc, "includePaths")?;
        if clean.is_empty() {
            return Err(AppError::InvalidArgument(
                "includePaths must not contain blank entries".to_owned(),
            ));
        }
        validated_includes.push(clean);
    }

    let safe_target = validate_restore_destination(target_dir)?;
    let cmd =
        build_restore_snapshot_command(config, secrets, snap, &safe_target, &validated_includes)?;
    std::fs::create_dir_all(&safe_target)?;

    run_to_end(cmd, "restore")?;
    Ok(format!(
        "Restored snapshot {snap} into {}",
        safe_target.display()
    ))
}

/// Parameters for a cross-PC handoff restore (`restic restore --target <dir> -- <id>:<subpath>`).
pub struct RestoreSubpathOptions<'a> {
    pub snapshot_id: &'a str,
    pub subpath: &'a str,
    pub target_dir: &'a Path,
    pub delete_extra: bool,
    pub exclude_patterns: &'a [String],
    pub retry_lock: &'a str,
    pub progress_label: &'a str,
}

fn build_restore_subpath_command(
    config: &AppConfig,
    secrets: &StoredSecrets,
    opts: &RestoreSubpathOptions<'_>,
    safe_target: &Path,
) -> AppResult<Command> {
    let snap = validate_snapshot_id(opts.snapshot_id, false)?;
    let subpath = validate_no_control_chars(opts.subpath, "subpath")?;
    if !subpath.starts_with('/') {
        return Err(AppError::InvalidArgument(
            "snapshotId and POSIX subpath starting with '/' are required".to_owned(),
        ));
    }
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(opts.retry_lock));
    cmd.arg("restore")
        .arg("--target")
        .arg(safe_target)
        .arg("--json");
    if opts.delete_extra {
        cmd.arg("--delete");
    }
    for pat in opts.exclude_patterns {
        let trimmed = pat.trim();
        if !trimmed.is_empty() {
            cmd.arg("--exclude").arg(trimmed);
        }
    }
    cmd.arg("--").arg(format!("{snap}:{subpath}"));
    Ok(cmd)
}

/// Restores a snapshot subpath directly into `target_dir` (absorbing drive/path differences across
/// PCs), optionally deleting extra files (`--delete`) while protecting excluded folders
/// (`--exclude`), with real-time progress, lock-wait detection, and cancellation.
///
/// Re-verifies that `opts.target_dir` still resolves to the exact approved canonical path (and is
/// not a swapped symlink/junction) right before spawning `restic restore`, and sets
/// `spawned_flag` only once `restic restore` has actually spawned.
pub fn restore_subpath(
    config: &AppConfig,
    secrets: &StoredSecrets,
    opts: &RestoreSubpathOptions<'_>,
    progress_state: &Arc<Mutex<BackupProgressState>>,
    cancel_flag: &Arc<AtomicBool>,
    spawned_flag: &AtomicBool,
) -> AppResult<String> {
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(AppError::Restic(
            "restore cancelled by user or agent".to_owned(),
        ));
    }
    let expected_canon = opts.target_dir.to_string_lossy();
    let safe_target = validate_restore_destination(opts.target_dir)?;
    if safe_target.to_string_lossy() != expected_canon {
        return Err(AppError::InvalidArgument(format!(
            "復元直前に復元先パスの実体が変化したため復元を中止しました（確認時 {expected_canon} → 現在 {}）",
            safe_target.display()
        )));
    }
    match std::fs::symlink_metadata(&safe_target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(AppError::InvalidArgument(format!(
                "復元先がシンボリックリンクまたはジャンクションに差し替えられているため復元を中止しました: {}",
                safe_target.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(AppError::Restic(format!(
                "復元先パスの状態確認に失敗したため復元を中止しました（{}）: {e}",
                safe_target.display()
            )));
        }
    }
    std::fs::create_dir_all(&safe_target)?;
    // Re-verify after `create_dir_all` that neither `safe_target` nor any of its ancestors is a
    // symlink/junction and that `canonicalize` still matches `expected_canon` exactly.
    for ancestor in safe_target.ancestors() {
        if ancestor.parent().is_none() {
            break;
        }
        let meta = std::fs::symlink_metadata(ancestor)?;
        if meta.file_type().is_symlink() {
            return Err(AppError::InvalidArgument(format!(
                "復元先または親フォルダがシンボリックリンク／ジャンクションのため復元を中止しました: {}",
                ancestor.display()
            )));
        }
    }
    let post_canon = dunce::canonicalize(&safe_target)?;
    if post_canon.to_string_lossy() != expected_canon {
        return Err(AppError::InvalidArgument(format!(
            "復元直前に復元先フォルダの実体パスが変化したため復元を中止しました（確認時 {expected_canon} → 現在 {}）",
            post_canon.display()
        )));
    }

    let mut cmd = build_restore_subpath_command(config, secrets, opts, &post_canon)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    if cancel_flag.load(Ordering::Relaxed) {
        return Err(AppError::Restic(
            "restore cancelled by user or agent".to_owned(),
        ));
    }

    let label = opts.progress_label.to_owned();
    if let Ok(mut guard) = progress_state.lock() {
        *guard = BackupProgressState {
            running: true,
            target_label: label.clone(),
            status_note: Some("引き継ぎデータを復元しています".to_owned()),
            ..Default::default()
        };
    }

    let mut child = cmd.spawn().map_err(|e| {
        if let Ok(mut guard) = progress_state.lock() {
            guard.running = false;
            guard.status_note = None;
        }
        AppError::Restic(format!("failed to spawn restic restore: {e}"))
    })?;
    spawned_flag.store(true, Ordering::Relaxed);

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Restic("missing child stdout".to_owned()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Restic("missing child stderr".to_owned()))?;

    let stderr_progress = Arc::clone(progress_state);
    let stderr_reader = std::thread::spawn(move || {
        let mut collected = String::new();
        for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
            if is_lock_wait_line(&line)
                && let Ok(mut guard) = stderr_progress.lock()
            {
                guard.waiting_for_lock = true;
                guard.status_note = Some(LOCK_WAIT_MESSAGE.to_owned());
            }
            if !collected.is_empty() {
                collected.push('\n');
            }
            collected.push_str(&line);
        }
        collected
    });

    let stdout_progress = Arc::clone(progress_state);
    let stdout_label = label.clone();
    let stdout_reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout_pipe).lines().map_while(Result::ok) {
            parse_restore_json_line(&line, &stdout_label, &stdout_progress);
        }
    });

    let status = loop {
        if cancel_flag.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            if let Ok(mut guard) = progress_state.lock() {
                guard.running = false;
                guard.waiting_for_lock = false;
                guard.status_note = None;
            }
            return Err(AppError::Restic(
                "restore cancelled by user or agent".to_owned(),
            ));
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                if let Ok(mut guard) = progress_state.lock() {
                    guard.running = false;
                    guard.waiting_for_lock = false;
                    guard.status_note = None;
                }
                return Err(AppError::Restic(format!(
                    "failed waiting on restic restore: {e}"
                )));
            }
        }
    };

    let _ = stdout_reader.join();
    let stderr = stderr_reader.join().unwrap_or_default();

    if let Ok(mut guard) = progress_state.lock() {
        guard.running = false;
        guard.waiting_for_lock = false;
        guard.status_note = None;
    }

    if !status.success() {
        return Err(classify_restic_error(status, "restore", &stderr));
    }

    Ok(format!(
        "Restored {}:{} into {}",
        opts.snapshot_id.trim(),
        opts.subpath.trim(),
        safe_target.display()
    ))
}

/// Checks whether `.git` exists anywhere inside `<subpath>` (root OR nested subdirectories such
/// as `<subpath>/vendor/Plugin/.git`) inside `snapshot_id`, so legacy untagged takeovers refuse
/// to extract Git repositories before touching the local filesystem.
pub fn snapshot_subpath_has_git(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_id: &str,
    subpath: &str,
    cancel_flag: &Arc<AtomicBool>,
) -> AppResult<bool> {
    let snap = validate_snapshot_id(snapshot_id, false)?;
    let clean_sub = validate_no_control_chars(subpath, "subpath")?.trim_end_matches('/');
    if !clean_sub.starts_with('/') {
        return Ok(false);
    }
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(RETRY_LOCK_READ));
    // `--recursive` is required whenever a path filter is passed to `restic ls`, otherwise
    // Restic only lists direct children of `clean_sub` and misses nested `.git` directories.
    cmd.arg("ls")
        .arg("--json")
        .arg("--recursive")
        .arg("--")
        .arg(snap)
        .arg(clean_sub);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    if cancel_flag.load(Ordering::Relaxed) {
        return Err(AppError::Restic(
            "restic ls cancelled by user or agent".to_owned(),
        ));
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::Restic(format!("failed to spawn restic ls: {e}")))?;
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Restic("missing restic ls stdout".to_owned()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Restic("missing restic ls stderr".to_owned()))?;

    let stderr_reader = std::thread::spawn(move || read_bounded_stderr(stderr_pipe));

    let found_git = Arc::new(AtomicBool::new(false));
    let found_git_reader = Arc::clone(&found_git);
    let sub_prefix = format!("{}/", clean_sub.to_ascii_lowercase());
    let stdout_reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout_pipe).lines().map_while(Result::ok) {
            let Ok(val) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if val.get("struct_type").and_then(Value::as_str) != Some("node") {
                continue;
            }
            let Some(path) = val.get("path").and_then(Value::as_str) else {
                continue;
            };
            let p_lower = path.trim_end_matches('/').to_ascii_lowercase();
            let Some(rel) = p_lower.strip_prefix(&sub_prefix) else {
                continue;
            };
            if rel.split('/').any(|seg| seg == ".git") {
                found_git_reader.store(true, Ordering::Relaxed);
                break;
            }
        }
    });

    let status = loop {
        if found_git.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Ok(true);
        }
        if cancel_flag.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(AppError::Restic(
                "restic ls cancelled by user or agent".to_owned(),
            ));
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(AppError::Restic(format!(
                    "failed waiting on restic ls: {e}"
                )));
            }
        }
    };

    let _ = stdout_reader.join();
    let stderr_bytes = stderr_reader.join().unwrap_or_default();
    if found_git.load(Ordering::Relaxed) {
        return Ok(true);
    }
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        return Err(classify_restic_error(status, "ls", &stderr));
    }
    Ok(false)
}

/// Summary of one snapshot group inside a multi-PC `forget --prune` plan/result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgetGroupPlan {
    pub group_label: String,
    pub keep_count: usize,
    pub remove_count: usize,
    pub protected_safety_count: usize,
    pub remove_short_ids: Vec<String>,
}

/// Complete multi-PC retention plan separating shared `project:<key>` effective snapshots,
/// `handoff:safety-backup` snapshots (protected until manual deletion without consuming retention
/// slots), and legacy/local-only `(host, paths)` groups (isolated from `project:*` snapshots).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiPcForgetPlan {
    pub total_keep: usize,
    pub total_remove: usize,
    pub protected_safety_backups: usize,
    pub remove_ids: Vec<String>,
    pub shared_projects: Vec<ForgetGroupPlan>,
    pub legacy_groups: Vec<ForgetGroupPlan>,
}

/// Applies `RetentionPolicy` (`keep_last`, `keep_daily`, `keep_weekly`, `keep_monthly`) to a
/// single isolated group of snapshots, matching Restic 0.17.3 `internal/restic/snapshot_policy.go`
/// (including the oldest-snapshot bucket fallback when fewer distinct calendar buckets exist than
/// the configured `keep_daily` / `keep_weekly` / `keep_monthly` limit).
fn apply_retention_to_group(
    group: &[&ResticSnapshot],
    policy: &crate::config::RetentionPolicy,
    guarantee_latest: bool,
) -> (Vec<String>, Vec<String>) {
    use chrono::{DateTime, Datelike};

    let mut ordered: Vec<&ResticSnapshot> = group.to_vec();
    ordered.sort_by(|a, b| crate::sync::compare_snapshot_time(b, a));
    let total = ordered.len();

    let mut kept: BTreeSet<String> = BTreeSet::new();
    let mut daily_kept = 0usize;
    let mut last_day: Option<(i32, u32)> = None;
    let mut weekly_kept = 0usize;
    let mut last_week: Option<(i32, u32)> = None;
    let mut monthly_kept = 0usize;
    let mut last_month: Option<(i32, u32)> = None;

    for (idx, snap) in ordered.iter().enumerate() {
        let is_oldest = idx + 1 == total;
        let mut keep = false;
        if guarantee_latest && idx == 0 {
            keep = true;
        }
        if idx < policy.keep_last as usize {
            keep = true;
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(snap.time.trim()) {
            let day_key = (dt.year(), dt.ordinal());
            if daily_kept < policy.keep_daily as usize && (last_day != Some(day_key) || is_oldest) {
                keep = true;
                last_day = Some(day_key);
                daily_kept += 1;
            }
            let iso = dt.iso_week();
            let week_key = (iso.year(), iso.week());
            if weekly_kept < policy.keep_weekly as usize
                && (last_week != Some(week_key) || is_oldest)
            {
                keep = true;
                last_week = Some(week_key);
                weekly_kept += 1;
            }
            let month_key = (dt.year(), dt.month());
            if monthly_kept < policy.keep_monthly as usize
                && (last_month != Some(month_key) || is_oldest)
            {
                keep = true;
                last_month = Some(month_key);
                monthly_kept += 1;
            }
        } else {
            // Unparseable timestamp: keep defensively rather than risk unexpected deletion.
            keep = true;
        }
        if keep {
            kept.insert(snap.id.clone());
        }
    }

    let mut keep_ids = Vec::new();
    let mut remove_ids = Vec::new();
    for snap in ordered {
        if kept.contains(&snap.id) {
            keep_ids.push(snap.id.clone());
        } else {
            remove_ids.push(snap.id.clone());
        }
    }
    (keep_ids, remove_ids)
}

/// Computes an isolated, slot-safe retention plan across all snapshots in the repository.
pub fn plan_multi_pc_forget(
    snaps: &[ResticSnapshot],
    policy: &crate::config::RetentionPolicy,
) -> MultiPcForgetPlan {
    use std::collections::BTreeMap;

    let mut project_effective: BTreeMap<String, Vec<&ResticSnapshot>> = BTreeMap::new();
    let mut project_safety: BTreeMap<String, usize> = BTreeMap::new();
    let mut legacy_groups_map: BTreeMap<(String, Vec<String>), Vec<&ResticSnapshot>> =
        BTreeMap::new();
    let mut total_safety = 0usize;

    for snap in snaps {
        if snap.is_safety_backup() {
            total_safety += 1;
            if let Some(key) = snap.project_key() {
                *project_safety.entry(key.to_owned()).or_default() += 1;
                project_effective.entry(key.to_owned()).or_default();
            }
            continue;
        }
        // Unpublished handoff staging snapshots are never part of effective retention groups.
        if snap.is_handoff_staging() {
            continue;
        }
        if let Some(key) = snap.project_key() {
            project_effective
                .entry(key.to_owned())
                .or_default()
                .push(snap);
        } else {
            // Match Restic's exact `--group-by host,paths` semantics (preserve path spelling/case!).
            let host = snap.hostname.clone();
            let mut paths = snap.paths.clone();
            paths.sort();
            legacy_groups_map
                .entry((host, paths))
                .or_default()
                .push(snap);
        }
    }

    let mut total_keep = total_safety;
    let mut total_remove = 0usize;
    let mut remove_ids = Vec::new();
    let mut shared_projects = Vec::new();

    for (key, group) in &project_effective {
        let safety_count = project_safety.get(key).copied().unwrap_or(0);
        let (keep, remove) = apply_retention_to_group(group, policy, true);
        let short_map: HashMap<&str, &str> = group
            .iter()
            .map(|s| (s.id.as_str(), s.display_short_id()))
            .collect();
        let remove_short_ids: Vec<String> = remove
            .iter()
            .map(|id| {
                short_map
                    .get(id.as_str())
                    .copied()
                    .unwrap_or(&id[..id.len().min(8)])
                    .to_owned()
            })
            .collect();
        total_keep += keep.len();
        total_remove += remove.len();
        remove_ids.extend(remove);
        shared_projects.push(ForgetGroupPlan {
            group_label: format!("project:{key}"),
            keep_count: keep.len(),
            remove_count: remove_short_ids.len(),
            protected_safety_count: safety_count,
            remove_short_ids,
        });
    }

    let mut legacy_groups = Vec::new();
    for ((host, paths), group) in &legacy_groups_map {
        let (keep, remove) = apply_retention_to_group(group, policy, false);
        let short_map: HashMap<&str, &str> = group
            .iter()
            .map(|s| (s.id.as_str(), s.display_short_id()))
            .collect();
        let remove_short_ids: Vec<String> = remove
            .iter()
            .map(|id| {
                short_map
                    .get(id.as_str())
                    .copied()
                    .unwrap_or(&id[..id.len().min(8)])
                    .to_owned()
            })
            .collect();
        total_keep += keep.len();
        total_remove += remove.len();
        remove_ids.extend(remove);
        let label = format!(
            "{} ({})",
            if host.is_empty() { "-" } else { host },
            if paths.is_empty() {
                "-".to_owned()
            } else {
                paths.join(", ")
            }
        );
        legacy_groups.push(ForgetGroupPlan {
            group_label: label,
            keep_count: keep.len(),
            remove_count: remove_short_ids.len(),
            protected_safety_count: 0,
            remove_short_ids,
        });
    }

    MultiPcForgetPlan {
        total_keep,
        total_remove,
        protected_safety_backups: total_safety,
        remove_ids,
        shared_projects,
        legacy_groups,
    }
}

fn build_forget_ids_command(
    config: &AppConfig,
    secrets: &StoredSecrets,
    ids: &[String],
    retry_lock: &str,
) -> AppResult<Command> {
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(retry_lock));
    cmd.arg("forget").arg("--json").arg("--");
    for id in ids {
        let valid = validate_snapshot_id(id, false)?;
        cmd.arg(valid);
    }
    Ok(cmd)
}

fn build_prune_command(
    config: &AppConfig,
    secrets: &StoredSecrets,
    retry_lock: &str,
) -> AppResult<Command> {
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(retry_lock));
    cmd.arg("prune");
    Ok(cmd)
}

/// Apply slot-safe multi-PC retention policy across shared projects (`project:<key>`, combining
/// all hosts while keeping `handoff:safety-backup` immune and never letting it consume retention
/// slots) and isolated legacy/local-only `(host, paths)` groups, followed by a single
/// `restic prune` when `dry_run` is false.
pub fn forget_and_prune_with_retry(
    config: &AppConfig,
    secrets: &StoredSecrets,
    dry_run: bool,
    retry_lock: &str,
) -> AppResult<String> {
    let r = &config.retention;
    if r.keep_last == 0 && r.keep_daily == 0 && r.keep_weekly == 0 && r.keep_monthly == 0 {
        return Ok(json!({
            "skipped": true,
            "dryRun": dry_run,
            "totalKeep": 0,
            "totalRemove": 0,
            "protectedSafetyBackups": 0,
            "reason": "all retention counts are 0"
        })
        .to_string());
    }

    let snaps = list_snapshots_filtered(config, secrets, None, None, None, Some(retry_lock))?;
    let plan = plan_multi_pc_forget(&snaps, r);

    let mut forget_outputs = Vec::new();
    if !dry_run {
        // Chunk snapshot IDs to stay well below Windows command-line length limits.
        for chunk in plan.remove_ids.chunks(50) {
            let cmd = build_forget_ids_command(config, secrets, chunk, retry_lock)?;
            let out = run_to_end(cmd, "forget")?;
            if !out.is_empty() {
                forget_outputs
                    .push(serde_json::from_str::<Value>(&out).unwrap_or(Value::String(out)));
            }
        }
    }

    let prune_output = if !dry_run && !snaps.is_empty() {
        let cmd = build_prune_command(config, secrets, retry_lock)?;
        Some(run_to_end(cmd, "prune")?)
    } else {
        None
    };

    Ok(serde_json::to_string(&json!({
        "dryRun": dry_run,
        "totalKeep": plan.total_keep,
        "totalRemove": plan.total_remove,
        "protectedSafetyBackups": plan.protected_safety_backups,
        "removeIds": plan.remove_ids,
        "sharedProjects": plan.shared_projects,
        "legacyGroups": plan.legacy_groups,
        "forgetOutputs": forget_outputs,
        "pruneOutput": prune_output,
    }))?)
}

/// Permanently forgets a single snapshot by ID (e.g. manually deleting a `handoff:safety-backup`
/// snapshot from the UI). Does not run `prune` so secondary PCs do not hold an extended prune lock.
pub fn forget_snapshot(
    config: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_id: &str,
) -> AppResult<String> {
    let snap = validate_snapshot_id(snapshot_id, false)?;
    let mut cmd = build_restic_command(config, secrets)?;
    apply_retry_lock(&mut cmd, Some(RETRY_LOCK_MANUAL_OR_HANDOFF));
    cmd.arg("forget").arg("--").arg(snap);
    run_to_end(cmd, "forget")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restic::types::TAG_HANDOFF_SAFETY_BACKUP;

    #[test]
    fn parse_backup_json_line_extracts_status_and_summary() {
        let progress = Arc::new(Mutex::new(BackupProgressState::default()));
        let mut summary = ResticBackupSummary::default();

        let status_line = r#"{"message_type":"status","percent_done":0.42,"files_done":12,"total_files":30,"bytes_done":1024,"total_bytes":4096,"current_files":["D:\\workspaces\\doc.md"]}"#;
        parse_backup_json_line(status_line, "Workspaces", &progress, &mut summary);

        {
            let guard = progress.lock().unwrap();
            assert!(guard.running);
            assert_eq!(guard.files_done, 12);
            assert_eq!(guard.current_file, r"D:\workspaces\doc.md");
        }

        let summary_line = r#"{"message_type":"summary","files_new":3,"files_changed":2,"files_unmodified":25,"data_added":51200,"total_files_processed":30,"total_bytes_processed":409600,"total_duration":1.25,"snapshot_id":"a1b2c3d4"}"#;
        parse_backup_json_line(summary_line, "Workspaces", &progress, &mut summary);

        assert_eq!(summary.files_new, 3);
        assert_eq!(summary.data_added, 51200);
        assert_eq!(summary.snapshot_id.as_deref(), Some("a1b2c3d4"));
    }

    #[test]
    fn unreadable_items_lists_each_item_from_restic_error_lines_once() {
        // Shape of restic 0.17 stderr for an access-denied folder (exit code 3).
        let stderr = r#"{"message_type":"error","error":{"message":"openfile for readdirnames failed: Access is denied."},"during":"scan","item":"D:\\p\\.pytest_cache"}
{"message_type":"error","error":{"message":"incomplete metadata"},"during":"archival","item":"D:\\p\\.pytest_cache"}
{"message_type":"error","error":{"message":"Access is denied."},"during":"archival","item":"D:\\p\\locked.bin"}
Warning: at least one source file could not be read"#;
        assert_eq!(
            unreadable_items(stderr),
            vec![
                r"D:\p\.pytest_cache".to_owned(),
                r"D:\p\locked.bin".to_owned()
            ]
        );
    }

    fn target(path: &str) -> BackupTarget {
        BackupTarget {
            id: path.to_owned(),
            label: path.to_owned(),
            path: path.to_owned(),
            enabled: true,
            project_key: None,
            writer_state: crate::config::WriterState::Active,
            exclude_patterns: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn added_tally_groups_new_data_two_levels_below_each_target() {
        let targets = [target(r"D:\Projects\ProjectAlpha"), target(r"E:\Media")];
        let mut tally = AddedTally {
            targets: &targets,
            bytes: HashMap::new(),
        };
        for line in [
            r#"{"message_type":"verbose_status","action":"new","item":"/D/Projects/ProjectAlpha/_Backups/20261109_x/a/scene.unity","data_size":900,"data_size_in_repo":700}"#,
            r#"{"message_type":"verbose_status","action":"modified","item":"/D/Projects/ProjectAlpha/_Backups/20261109_x/b.asset","data_size":400,"data_size_in_repo":300}"#,
            r#"{"message_type":"verbose_status","action":"new","item":"/D/Projects/ProjectAlpha/root.txt","data_size":5,"data_size_in_repo":50}"#,
            r#"{"message_type":"verbose_status","action":"modified","item":"/e/Media/Clips/","data_size":0,"data_size_in_repo":999}"#,
            r#"{"message_type":"verbose_status","action":"new","item":"/E/Media/Clips/clip.dat","data_size":10,"data_size_in_repo":0}"#,
            r#"{"message_type":"verbose_status","action":"new","item":"/C/elsewhere/x.bin","data_size":1,"data_size_in_repo":5000}"#,
        ] {
            tally.add_line(line);
        }
        assert_eq!(
            tally.top(5),
            vec![
                FolderBytes {
                    path: r"D:\Projects\ProjectAlpha\_Backups\20261109_x".to_owned(),
                    bytes: 1000,
                },
                FolderBytes {
                    path: r"D:\Projects\ProjectAlpha".to_owned(),
                    bytes: 50,
                },
            ]
        );
    }

    #[test]
    fn ls_file_size_reads_files_and_skips_folders() {
        assert_eq!(
            ls_file_size(
                r#"{"name":"a.png","type":"file","path":"/D/proj/Assets/a.png","size":1200,"struct_type":"node"}"#
            ),
            Some(("/D/proj/Assets/a.png".to_owned(), 1200))
        );
        assert_eq!(
            ls_file_size(
                r#"{"name":"Assets","type":"dir","path":"/D/proj/Assets","struct_type":"node"}"#
            ),
            None
        );
        assert_eq!(
            ls_file_size(r#"{"struct_type":"snapshot","id":"abc"}"#),
            None
        );
    }

    #[test]
    fn unreadable_items_falls_back_to_restic_warning_without_json() {
        assert_eq!(
            unreadable_items("Warning: at least one source file could not be read\n"),
            vec!["Warning: at least one source file could not be read".to_owned()]
        );
    }

    fn test_r2_config_and_secrets() -> (AppConfig, StoredSecrets) {
        let mut cfg = AppConfig {
            machine_name: Some("PC-B".to_owned()),
            ..Default::default()
        };
        cfg.r2.account_id = "acc123".to_owned();
        cfg.r2.bucket = "example-bucket".to_owned();
        cfg.r2.prefix = "example-prefix".to_owned();
        let mut secrets = StoredSecrets::default();
        secrets.r2_access_key_id = Some("ak".to_owned());
        secrets.r2_secret_access_key = Some("sk".to_owned());
        secrets.restic_password = Some("pw".to_owned());
        (cfg, secrets)
    }

    #[test]
    fn multi_pc_restic_commands_set_host_retry_lock_tags_and_grouping() {
        use std::ffi::OsStr;

        let (cfg, secrets) = test_r2_config_and_secrets();
        let mut t = target(r"E:\Projects\ProjectAlpha");
        t.project_key = Some("project-alpha".to_owned());
        t.tags = vec!["custom".to_owned()];

        let opts = BackupTargetOptions {
            dry_run: false,
            extra_tags: &["custom".to_owned()],
            internal_tags: &[crate::restic::types::TAG_HANDOFF_RELEASED.to_owned()],
            suppress_project_tag: false,
            retry_lock: RETRY_LOCK_SCHEDULED_BACKUP,
            parent_snapshot: Some("abc12345"),
            progress_label: None,
        };
        let (cmd, fp) = build_backup_target_command(&cfg, &secrets, &t, &opts).unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["--retry-lock", "10m"]));
        assert!(args.windows(2).any(|w| w == ["--host", "pc-b"]));
        assert!(args.windows(2).any(|w| w == ["--parent", "abc12345"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--tag", "project:project-alpha"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--tag", &format!("excludes:{fp}")])
        );
        assert!(args.windows(2).any(|w| w == ["--tag", "handoff:released"]));
        assert_eq!(args.iter().filter(|a| *a == "custom").count(), 1);

        // Free-form user/MCP tags cannot inject reserved namespaces or commas.
        for bad_tag in [
            "handoff:released",
            "project:other",
            "excludes:deadbeef",
            "ok,handoff:released",
        ] {
            let bad_opts = BackupTargetOptions {
                dry_run: false,
                extra_tags: &[bad_tag.to_owned()],
                internal_tags: &[],
                suppress_project_tag: false,
                retry_lock: RETRY_LOCK_MANUAL_OR_HANDOFF,
                parent_snapshot: None,
                progress_label: None,
            };
            assert!(
                build_backup_target_command(&cfg, &secrets, &t, &bad_opts).is_err(),
                "reserved or comma tag {bad_tag} must be rejected"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let restore_opts = RestoreSubpathOptions {
            snapshot_id: "a1b2c3d4",
            subpath: "/D/Projects/ProjectAlpha",
            target_dir: dir.path(),
            delete_extra: true,
            exclude_patterns: &["/Library".to_owned(), "/Temp".to_owned()],
            retry_lock: RETRY_LOCK_MANUAL_OR_HANDOFF,
            progress_label: "ProjectAlpha",
        };
        let rcmd =
            build_restore_subpath_command(&cfg, &secrets, &restore_opts, dir.path()).unwrap();
        let rargs: Vec<String> = rcmd
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect();
        assert!(rargs.windows(2).any(|w| w == ["--retry-lock", "2m"]));
        assert!(
            rargs
                .windows(2)
                .any(|w| w == ["--", "a1b2c3d4:/D/Projects/ProjectAlpha"])
        );
        assert!(rargs.contains(&"--delete".to_owned()));
        assert!(rargs.windows(2).any(|w| w == ["--exclude", "/Library"]));

        // Slot-safe retention plan: keep_last=1 with a newer safety-backup keeps both the
        // safety-backup and the newest effective snapshot; new shared snapshot with same (host,path)
        // must not consume legacy snapshot's retention slot.
        let make_snap =
            |id: &str, time: &str, host: &str, path: &str, tags: &[&str]| ResticSnapshot {
                id: id.to_owned(),
                short_id: id[..id.len().min(8)].to_owned(),
                original: None,
                time: time.to_owned(),
                hostname: host.to_owned(),
                username: "u".to_owned(),
                paths: vec![path.to_owned()],
                tags: tags.iter().map(|s| (*s).to_owned()).collect(),
            };
        let policy = crate::config::RetentionPolicy {
            keep_last: 1,
            keep_daily: 0,
            keep_weekly: 0,
            keep_monthly: 0,
        };
        let plan = plan_multi_pc_forget(
            &[
                make_snap(
                    "1111aaaa",
                    "2026-11-20T05:00:00+09:00",
                    "pc-a",
                    r"D:\Proj",
                    &[],
                ),
                make_snap(
                    "2222bbbb",
                    "2026-11-21T05:00:00+09:00",
                    "pc-a",
                    r"D:\Proj",
                    &["project:project-alpha"],
                ),
                make_snap(
                    "3333cccc",
                    "2026-11-22T05:00:00+09:00",
                    "pc-b",
                    r"E:\Proj",
                    &["project:project-alpha"],
                ),
                make_snap(
                    "4444dddd",
                    "2026-11-23T05:00:00+09:00",
                    "pc-a",
                    r"D:\Proj",
                    &["project:project-alpha", TAG_HANDOFF_SAFETY_BACKUP],
                ),
            ],
            &policy,
        );
        assert_eq!(plan.protected_safety_backups, 1);
        assert_eq!(plan.remove_ids, vec!["2222bbbb".to_owned()]);
        assert_eq!(plan.total_keep, 3);

        // Same-day snapshots with keep_daily=2 keep newest and oldest (matching Restic 0.17.3).
        let daily_policy = crate::config::RetentionPolicy {
            keep_last: 0,
            keep_daily: 2,
            keep_weekly: 0,
            keep_monthly: 0,
        };
        let daily_plan_2 = plan_multi_pc_forget(
            &[
                make_snap("d1", "2026-11-20T10:00:00+09:00", "pc-a", r"D:\A", &[]),
                make_snap("d2", "2026-11-20T12:00:00+09:00", "pc-a", r"D:\A", &[]),
            ],
            &daily_policy,
        );
        assert!(daily_plan_2.remove_ids.is_empty());
        let daily_plan_3 = plan_multi_pc_forget(
            &[
                make_snap("d1", "2026-11-20T10:00:00+09:00", "pc-a", r"D:\A", &[]),
                make_snap("d2", "2026-11-20T12:00:00+09:00", "pc-a", r"D:\A", &[]),
                make_snap("d3", "2026-11-20T14:00:00+09:00", "pc-a", r"D:\A", &[]),
            ],
            &daily_policy,
        );
        assert_eq!(daily_plan_3.remove_ids, vec!["d2".to_owned()]);

        // Legacy `(host, paths)` grouping preserves exact path case (`D:\A` vs `D:\a`).
        let case_plan = plan_multi_pc_forget(
            &[
                make_snap("upper_a", "2026-11-20T10:00:00+09:00", "pc-a", r"D:\A", &[]),
                make_snap("lower_a", "2026-11-20T12:00:00+09:00", "pc-a", r"D:\a", &[]),
            ],
            &policy,
        );
        assert!(case_plan.remove_ids.is_empty());
        assert_eq!(case_plan.legacy_groups.len(), 2);

        let fcmd =
            build_forget_ids_command(&cfg, &secrets, &plan.remove_ids, RETRY_LOCK_MAINTENANCE)
                .unwrap();
        let fargs: Vec<String> = fcmd
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect();
        assert!(fargs.contains(&"forget".to_owned()));
        assert!(fargs.windows(2).any(|w| w == ["--", "2222bbbb"]));
        assert!(!fargs.contains(&"4444dddd".to_owned()));

        let ucmd = build_unlock_command(&cfg, &secrets).unwrap();
        let uargs: Vec<String> = ucmd
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect();
        assert!(uargs.contains(&"unlock".to_owned()));
        assert!(!uargs.contains(&"--remove-all".to_owned()));
    }

    #[test]
    fn lock_wait_and_restore_progress_helpers_parse_expected_lines() {
        assert!(is_lock_wait_line(
            "repository is already locked exclusively by PID 123 on pc-a; retrying lock"
        ));
        assert!(is_lock_wait_line("unable to create lock in backend"));
        assert!(!is_lock_wait_line("Warning: file could not be read"));

        assert!(!is_item_within_target_subpath(
            "/D/Projects/",
            "/D/Projects/ProjectAlpha"
        ));
        assert!(!is_item_within_target_subpath(
            "/D/Projects/ProjectAlpha2/",
            "/D/Projects/ProjectAlpha"
        ));
        assert!(is_item_within_target_subpath(
            "/D/Projects/ProjectAlpha/",
            "/D/Projects/ProjectAlpha"
        ));
        assert!(is_item_within_target_subpath(
            "/d/projects/projectalpha/Assets/",
            "/D/Projects/ProjectAlpha"
        ));

        let progress = Arc::new(Mutex::new(BackupProgressState::default()));
        parse_restore_json_line(
            r#"{"message_type":"status","percent_done":0.5,"files_restored":10,"total_files":20,"bytes_restored":500,"total_bytes":1000}"#,
            "ProjectAlpha",
            &progress,
        );
        let guard = progress.lock().unwrap();
        assert!(guard.running);
        assert!(!guard.waiting_for_lock);
        assert_eq!(guard.files_done, 10);
        assert_eq!(guard.total_files, 20);
    }

    #[test]
    fn snapshot_id_validation_and_double_dash_prevent_option_injection() {
        use std::ffi::OsStr;

        assert_eq!(validate_snapshot_id("latest", true).unwrap(), "latest");
        assert!(validate_snapshot_id("latest", false).is_err());
        assert_eq!(validate_snapshot_id("deadbeef", true).unwrap(), "deadbeef");
        assert_eq!(
            validate_snapshot_id(&"a".repeat(64), false).unwrap(),
            "a".repeat(64)
        );
        for bad in [
            "",
            "   ",
            "abc",
            "-v",
            "--password-command=calc.exe",
            "--option=foo",
            "deadbeef:sub",
            "zzzz1234",
            &"a".repeat(65),
        ] {
            assert!(
                validate_snapshot_id(bad, true).is_err(),
                "expected invalid snapshotId {bad:?} to be rejected"
            );
        }

        let (cfg, secrets) = test_r2_config_and_secrets();
        // Option-like snapshot IDs are rejected before command creation.
        assert!(
            build_list_snapshot_files_command(&cfg, &secrets, "--password-command=calc", None)
                .is_err()
        );
        assert!(
            build_diff_snapshots_command(&cfg, &secrets, "latest", "--password-command=calc")
                .is_err()
        );
        assert!(
            build_snapshot_folder_sizes_command(&cfg, &secrets, "--password-command=calc").is_err()
        );

        // Even a `-`-prefixed prefix argument is placed strictly after `--` so Restic cannot parse
        // it as a flag.
        let ls_cmd = build_list_snapshot_files_command(
            &cfg,
            &secrets,
            "latest",
            Some("--password-command=calc"),
        )
        .unwrap();
        let ls_args: Vec<String> = ls_cmd
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect();
        assert!(
            ls_args
                .windows(3)
                .any(|w| w == ["--", "latest", "--password-command=calc"]),
            "expected `--` before positional args, got: {ls_args:?}"
        );

        let (diff_cmd, _, _) =
            build_diff_snapshots_command(&cfg, &secrets, "aaaa1111", "bbbb2222").unwrap();
        let diff_args: Vec<String> = diff_cmd
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect();
        assert!(
            diff_args
                .windows(3)
                .any(|w| w == ["--", "aaaa1111", "bbbb2222"]),
            "expected `--` before diff snapshot IDs, got: {diff_args:?}"
        );

        let dir = tempfile::tempdir().unwrap();
        let restore_cmd = build_restore_snapshot_command(
            &cfg,
            &secrets,
            "deadbeef",
            dir.path(),
            &["--not-a-flag"],
        )
        .unwrap();
        let restore_args: Vec<String> = restore_cmd
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect();
        assert!(restore_args.contains(&"--include=--not-a-flag".to_owned()));
        assert!(
            restore_args.windows(2).any(|w| w == ["--", "deadbeef"]),
            "expected `--` before restore snapshot ID, got: {restore_args:?}"
        );
    }

    #[test]
    fn parse_ls_entries_streaming_stops_at_cap_without_reading_remaining_stream() {
        struct FailAfterChunks {
            chunks: Vec<&'static [u8]>,
            idx: usize,
        }
        impl Read for FailAfterChunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.idx >= self.chunks.len() {
                    panic!("streaming reader must stop after reaching cap without reading more");
                }
                let data = self.chunks[self.idx];
                self.idx += 1;
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
        }

        let reader = BufReader::with_capacity(
            128,
            FailAfterChunks {
                chunks: vec![
                    b"{\"struct_type\":\"snapshot\",\"id\":\"deadbeef\"}\n",
                    b"{\"struct_type\":\"node\",\"path\":\"/a\",\"type\":\"file\",\"size\":10}\n",
                    b"{\"struct_type\":\"node\",\"path\":\"/b\",\"type\":\"file\",\"size\":20}\n",
                ],
                idx: 0,
            },
        );
        let (entries, stopped_early) = parse_ls_entries_streaming(reader, 2);
        assert!(stopped_early);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/a");
        assert_eq!(entries[1].path, "/b");
    }

    #[test]
    fn restore_snapshot_rejects_blank_include_or_flag_id_before_creating_destination() {
        let (cfg, secrets) = test_r2_config_and_secrets();
        let dir = tempfile::tempdir().unwrap();
        let non_existent = dir.path().join("must-not-be-created");

        assert!(
            restore_snapshot(&cfg, &secrets, "latest", &non_existent, &["   ".to_owned()]).is_err()
        );
        assert!(
            !non_existent.exists(),
            "destination must not be created when includePaths contains a blank filter"
        );

        assert!(
            restore_snapshot(
                &cfg,
                &secrets,
                "--password-command=calc",
                &non_existent,
                &[]
            )
            .is_err()
        );
        assert!(
            !non_existent.exists(),
            "destination must not be created when snapshotId is invalid"
        );
    }
}
